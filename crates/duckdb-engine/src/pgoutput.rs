//! A decoder for PostgreSQL's built-in `pgoutput` logical replication messages.
//!
//! `src.postgres.cdc` reads a replication slot through SQL
//! (`pg_logical_slot_peek_binary_changes`), one message per row. `pgoutput`
//! ships with PostgreSQL 10+ and every managed service, so nothing has to be
//! installed on the server, and no JVM or Kafka sits in between.
//!
//! The format is documented under "Logical Replication Message Formats" in the
//! PostgreSQL manual. Protocol version 1, text-format values: each column value
//! arrives as its text representation, which DuckDB then casts.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct RelColumn {
    pub name: String,
    pub type_oid: u32,
    /// Part of the replica identity (the key a DELETE is sent with).
    pub key: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub namespace: String,
    pub name: String,
    pub columns: Vec<RelColumn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Insert,
    Update,
    Delete,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Insert => "insert",
            Op::Update => "update",
            Op::Delete => "delete",
        }
    }
}

/// One row-level change, with the values aligned to its relation's columns.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub op: Op,
    pub relation_oid: u32,
    pub xid: u32,
    /// The transaction's commit LSN, from its Begin message.
    pub commit_lsn: u64,
    /// Microseconds since 2000-01-01 UTC, PostgreSQL's epoch.
    pub commit_ts_micros: i64,
    /// For a DELETE sent with only the replica identity, the non-key columns
    /// are None: the server did not send them, which is not the same as NULL.
    pub values: Vec<Option<String>>,
}

#[derive(Debug, Default)]
pub struct Decoder {
    relations: HashMap<u32, Relation>,
    xid: u32,
    commit_lsn: u64,
    commit_ts_micros: i64,
}

impl Decoder {
    pub fn relation(&self, oid: u32) -> Option<&Relation> {
        self.relations.get(&oid)
    }

    /// Decode one message. `Ok(None)` for the messages that carry no row -
    /// transaction boundaries, relation and type descriptions, origins.
    pub fn decode(&mut self, msg: &[u8]) -> Result<Option<Change>, String> {
        let mut r = Reader { buf: msg, at: 0 };
        match r.u8()? {
            b'B' => {
                self.commit_lsn = r.i64()? as u64;
                self.commit_ts_micros = r.i64()?;
                self.xid = r.i32()? as u32;
                Ok(None)
            }
            b'C' | b'O' | b'Y' | b'M' => Ok(None),
            b'R' => {
                let oid = r.i32()? as u32;
                let namespace = r.cstr()?;
                let name = r.cstr()?;
                let _replica_identity = r.u8()?;
                let n = r.i16()?.max(0) as usize;
                let mut columns = Vec::with_capacity(n);
                for _ in 0..n {
                    let flags = r.u8()?;
                    let name = r.cstr()?;
                    let type_oid = r.i32()? as u32;
                    let _typmod = r.i32()?;
                    columns.push(RelColumn { name, type_oid, key: flags & 1 == 1 });
                }
                self.relations.insert(oid, Relation { namespace, name, columns });
                Ok(None)
            }
            b'I' => {
                let oid = r.i32()? as u32;
                self.expect(&mut r, b'N')?;
                let new = r.tuple()?;
                self.change(Op::Insert, oid, new, None)
            }
            b'U' => {
                let oid = r.i32()? as u32;
                let mut old = None;
                let mut tag = r.u8()?;
                if tag == b'K' || tag == b'O' {
                    old = Some((tag, r.tuple()?));
                    tag = r.u8()?;
                }
                if tag != b'N' {
                    return Err(format!("pgoutput: UPDATE expected a new tuple, found {:?}", tag as char));
                }
                let new = r.tuple()?;
                // Only a FULL old image ('O') holds every column; a key image
                // ('K') cannot stand in for a non-key value.
                let full_old = old.filter(|(t, _)| *t == b'O').map(|(_, v)| v);
                self.change(Op::Update, oid, new, full_old)
            }
            b'D' => {
                let oid = r.i32()? as u32;
                let tag = r.u8()?;
                if tag != b'K' && tag != b'O' {
                    return Err(format!("pgoutput: DELETE expected a key or old tuple, found {:?}", tag as char));
                }
                let old = r.tuple()?;
                self.change(Op::Delete, oid, old, None)
            }
            b'T' => {
                let n = r.i32()?.max(0) as usize;
                let _options = r.u8()?;
                let mut names = Vec::with_capacity(n);
                for _ in 0..n {
                    let oid = r.i32()? as u32;
                    names.push(match self.relations.get(&oid) {
                        Some(rel) => format!("{}.{}", rel.namespace, rel.name),
                        None => format!("relation {oid}"),
                    });
                }
                Err(format!(
                    "pgoutput: TRUNCATE of {} cannot be expressed as row changes. Publish only \
                     insert, update and delete (the publication Duckle creates does), and reload \
                     the table if it really was emptied",
                    names.join(", ")
                ))
            }
            other => Err(format!("pgoutput: unsupported message type {:?}", other as char)),
        }
    }

    fn expect(&self, r: &mut Reader<'_>, tag: u8) -> Result<(), String> {
        match r.u8()? {
            t if t == tag => Ok(()),
            t => Err(format!("pgoutput: expected {:?}, found {:?}", tag as char, t as char)),
        }
    }

    /// Resolve a tuple against its relation. An unchanged TOASTed value was not
    /// sent; it is taken from the full old image when there is one, and
    /// refused otherwise - a NULL in its place would let a downstream upsert
    /// erase the real value.
    fn change(
        &self,
        op: Op,
        oid: u32,
        tuple: Vec<Value>,
        old: Option<Vec<Value>>,
    ) -> Result<Option<Change>, String> {
        let rel = self
            .relations
            .get(&oid)
            .ok_or_else(|| format!("pgoutput: a change for relation {oid} arrived before its description"))?;
        let mut values = Vec::with_capacity(tuple.len());
        for (i, v) in tuple.into_iter().enumerate() {
            values.push(match v {
                Value::Null => None,
                Value::Text(s) => Some(s),
                Value::Unchanged => match old.as_ref().and_then(|o| o.get(i)) {
                    Some(Value::Text(s)) => Some(s.clone()),
                    Some(Value::Null) => None,
                    _ => {
                        let col = rel.columns.get(i).map(|c| c.name.as_str()).unwrap_or("?");
                        return Err(format!(
                            "pgoutput: {}.{} column {col} was not sent on an UPDATE (an unchanged \
                             large value), and there is no old row image to take it from. Run \
                             ALTER TABLE {}.{} REPLICA IDENTITY FULL so updates carry every column",
                            rel.namespace, rel.name, rel.namespace, rel.name
                        ));
                    }
                },
            });
        }
        Ok(Some(Change {
            op,
            relation_oid: oid,
            xid: self.xid,
            commit_lsn: self.commit_lsn,
            commit_ts_micros: self.commit_ts_micros,
            values,
        }))
    }
}

/// An LSN in PostgreSQL's own text form, `X/Y` in hex.
pub fn format_lsn(lsn: u64) -> String {
    format!("{:X}/{:X}", lsn >> 32, lsn & 0xFFFF_FFFF)
}

enum Value {
    Null,
    Unchanged,
    Text(String),
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(n).filter(|e| *e <= self.buf.len()).ok_or_else(|| {
            format!("pgoutput: message ends early (wanted {n} bytes at {} of {})", self.at, self.buf.len())
        })?;
        let s = &self.buf[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn i16(&mut self) -> Result<i16, String> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn cstr(&mut self) -> Result<String, String> {
        let rest = &self.buf[self.at..];
        let n = rest.iter().position(|b| *b == 0).ok_or("pgoutput: unterminated string")?;
        let s = String::from_utf8_lossy(&rest[..n]).into_owned();
        self.at += n + 1;
        Ok(s)
    }
    fn tuple(&mut self) -> Result<Vec<Value>, String> {
        let n = self.i16()?.max(0) as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(match self.u8()? {
                b'n' => Value::Null,
                b'u' => Value::Unchanged,
                b't' => {
                    let len = self.i32()?;
                    let len = usize::try_from(len).map_err(|_| format!("pgoutput: negative length {len}"))?;
                    Value::Text(String::from_utf8_lossy(self.take(len)?).into_owned())
                }
                b'b' => return Err("pgoutput: binary values were not requested".into()),
                other => return Err(format!("pgoutput: unknown value kind {:?}", other as char)),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds messages byte for byte as the manual lays them out, so the
    /// tests check the decoder against the protocol, not against itself.
    struct Msg(Vec<u8>);
    impl Msg {
        fn new(tag: u8) -> Self {
            Msg(vec![tag])
        }
        fn u8(mut self, v: u8) -> Self {
            self.0.push(v);
            self
        }
        fn i16(mut self, v: i16) -> Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn i32(mut self, v: i32) -> Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn i64(mut self, v: i64) -> Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn cstr(mut self, s: &str) -> Self {
            self.0.extend_from_slice(s.as_bytes());
            self.0.push(0);
            self
        }
        /// TupleData: Int16 count, then per column n / u / t+Int32 len+bytes.
        fn tuple(mut self, cols: &[Option<Option<&str>>]) -> Self {
            self = self.i16(cols.len() as i16);
            for c in cols {
                self = match c {
                    None => self.u8(b'u'),
                    Some(None) => self.u8(b'n'),
                    Some(Some(v)) => {
                        let s = self.u8(b't').i32(v.len() as i32);
                        let mut s = s;
                        s.0.extend_from_slice(v.as_bytes());
                        s
                    }
                };
            }
            self
        }
    }

    fn begin(xid: i32, ts: i64) -> Vec<u8> {
        Msg::new(b'B').i64(0x16B3748).i64(ts).i32(xid).0
    }

    /// public.orders(id int4 key, note text), relation oid 16384.
    fn relation() -> Vec<u8> {
        Msg::new(b'R')
            .i32(16384)
            .cstr("public")
            .cstr("orders")
            .u8(b'd')
            .i16(2)
            .u8(1).cstr("id").i32(23).i32(-1)
            .u8(0).cstr("note").i32(25).i32(-1)
            .0
    }

    fn decoded(msgs: &[Vec<u8>]) -> (Decoder, Vec<Change>) {
        let mut d = Decoder::default();
        let mut out = Vec::new();
        for m in msgs {
            if let Some(c) = d.decode(m).expect("decodes") {
                out.push(c);
            }
        }
        (d, out)
    }

    #[test]
    fn a_relation_describes_its_columns_and_key() {
        let (d, _) = decoded(&[relation()]);
        let r = d.relation(16384).expect("relation kept");
        assert_eq!((r.namespace.as_str(), r.name.as_str()), ("public", "orders"));
        let cols: Vec<(&str, u32, bool)> =
            r.columns.iter().map(|c| (c.name.as_str(), c.type_oid, c.key)).collect();
        assert_eq!(cols, vec![("id", 23, true), ("note", 25, false)]);
    }

    #[test]
    fn inserts_updates_and_deletes_carry_their_transaction() {
        let insert = Msg::new(b'I').i32(16384).u8(b'N').tuple(&[Some(Some("1")), Some(Some("hello"))]).0;
        let update = Msg::new(b'U').i32(16384).u8(b'N').tuple(&[Some(Some("1")), Some(None)]).0;
        let delete = Msg::new(b'D').i32(16384).u8(b'K').tuple(&[Some(Some("1")), Some(None)]).0;
        let commit = Msg::new(b'C').u8(0).i64(1).i64(2).i64(3).0;
        let (_, changes) = decoded(&[begin(731, 42), relation(), insert, update, delete, commit]);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].op, Op::Insert);
        assert_eq!(changes[0].values, vec![Some("1".into()), Some("hello".into())]);
        assert_eq!(changes[1].op, Op::Update);
        assert_eq!(changes[1].values, vec![Some("1".into()), None], "an explicit NULL stays NULL");
        assert_eq!(changes[2].op, Op::Delete);
        assert_eq!(changes[2].values[0], Some("1".into()), "a delete names its key");
        assert!(changes.iter().all(|c| c.xid == 731 && c.commit_ts_micros == 42));
        // begin() writes final LSN 0x16B3748, which PostgreSQL prints as 0/16B3748.
        assert!(changes.iter().all(|c| format_lsn(c.commit_lsn) == "0/16B3748"));
    }

    /// An unchanged TOASTed value is not sent on an UPDATE. Emitting it as NULL
    /// would let a downstream upsert erase the real value, so it is filled from
    /// the old row image when the table sends one (REPLICA IDENTITY FULL)...
    #[test]
    fn an_unchanged_large_value_is_taken_from_the_old_row() {
        let update = Msg::new(b'U')
            .i32(16384)
            .u8(b'O')
            .tuple(&[Some(Some("1")), Some(Some("a very long note"))])
            .u8(b'N')
            .tuple(&[Some(Some("1")), None])
            .0;
        let (_, changes) = decoded(&[begin(1, 0), relation(), update]);
        assert_eq!(changes[0].values[1], Some("a very long note".into()));
    }

    /// ...and refused, naming the fix, when it does not. Never a silent NULL.
    #[test]
    fn an_unchanged_large_value_with_no_old_row_is_refused() {
        let update = Msg::new(b'U').i32(16384).u8(b'N').tuple(&[Some(Some("1")), None]).0;
        let mut d = Decoder::default();
        d.decode(&begin(1, 0)).unwrap();
        d.decode(&relation()).unwrap();
        let err = d.decode(&update).expect_err("must not become NULL");
        assert!(err.contains("REPLICA IDENTITY FULL"), "{err}");
        assert!(err.contains("note"), "names the column: {err}");
    }

    #[test]
    fn a_change_for_an_unknown_relation_is_an_error() {
        let insert = Msg::new(b'I').i32(99).u8(b'N').tuple(&[Some(Some("1"))]).0;
        let err = Decoder::default().decode(&insert).expect_err("no relation");
        assert!(err.contains("99"), "{err}");
    }

    /// A publication made by hand may publish TRUNCATE, which is not a row
    /// change. Refused loudly rather than dropped.
    #[test]
    fn a_truncate_is_refused_rather_than_dropped() {
        let truncate = Msg::new(b'T').i32(1).u8(0).i32(16384).0;
        let mut d = Decoder::default();
        d.decode(&relation()).unwrap();
        let err = d.decode(&truncate).expect_err("truncate");
        assert!(err.contains("orders") && err.contains("TRUNCATE"), "{err}");
    }

    #[test]
    fn a_truncated_message_is_an_error_not_a_panic() {
        let short = Msg::new(b'I').i32(16384).0;
        let mut d = Decoder::default();
        d.decode(&relation()).unwrap();
        assert!(d.decode(&short).is_err());
        assert!(Decoder::default().decode(&[]).is_err());
    }

    #[test]
    fn boundaries_and_descriptions_carry_no_row() {
        let origin = Msg::new(b'O').i64(5).cstr("node_a").0;
        let typ = Msg::new(b'Y').i32(700).cstr("public").cstr("mood").0;
        let commit = Msg::new(b'C').u8(0).i64(1).i64(2).i64(3).0;
        let (_, changes) = decoded(&[begin(1, 0), origin, typ, commit]);
        assert!(changes.is_empty());
    }
}
