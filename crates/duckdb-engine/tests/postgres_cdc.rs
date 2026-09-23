//! `src.postgres.cdc` against a real PostgreSQL.
//!
//! Needs a server with `wal_level=logical` and a role that may create
//! publications and replication slots, found through the same DUCKLE_PG_HOST /
//! _PORT / _DB / _USER / _PASS the engine's other live Postgres tests read, plus
//! DUCKLE_DUCKDB_BIN. Without them the tests skip. For a throwaway server:
//!
//!   docker run -d -p 55432:5432 -e POSTGRES_PASSWORD=pw postgres:16 -c wal_level=logical
//!   DUCKLE_PG_HOST=127.0.0.1 DUCKLE_PG_PORT=55432 DUCKLE_PG_PASS=pw
//!
//! Each test owns its table, publication and slot by name, and removes them
//! first, so a run that died halfway does not poison the next.

use duckle_duckdb_engine::{DuckdbEngine, PipelineDoc};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Mutex;

/// The workspace is process-global, and these tests each set their own.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Live {
    engine: DuckdbEngine,
    bin: String,
    dsn: String,
}

fn live() -> Option<Live> {
    let bin = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists())?;
    let host = std::env::var("DUCKLE_PG_HOST").ok().filter(|h| !h.trim().is_empty())?;
    let var = |k: &str, default: &str| std::env::var(k).unwrap_or_else(|_| default.to_string());
    let dsn = format!(
        "host={host} port={} dbname={} user={} password={}",
        var("DUCKLE_PG_PORT", "5432"),
        var("DUCKLE_PG_DB", "postgres"),
        var("DUCKLE_PG_USER", "postgres"),
        var("DUCKLE_PG_PASS", ""),
    );
    Some(Live { engine: DuckdbEngine::new(bin.clone().into()), bin, dsn })
}

macro_rules! live_or_skip {
    () => {
        match live() {
            Some(l) => l,
            None => {
                eprintln!("skipping: set DUCKLE_DUCKDB_BIN and DUCKLE_PG_HOST to run");
                return;
            }
        }
    };
}

impl Live {
    /// Run statements on the server, each in its own transaction, through the
    /// same DuckDB extension the engine uses. Failures are fatal: a test whose
    /// setup silently did not happen proves nothing.
    fn exec(&self, statements: &[&str]) {
        for sql in statements {
            let script = format!(
                "LOAD postgres; ATTACH '{}' AS pg (TYPE postgres); CALL postgres_execute('pg', '{}');",
                self.dsn.replace('\'', "''"),
                sql.replace('\'', "''")
            );
            let out = std::process::Command::new(&self.bin)
                .args([":memory:", "-c", &script])
                .output()
                .expect("duckdb runs");
            assert!(
                out.status.success(),
                "setup `{sql}` failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// Drop whatever an earlier run of this test left behind.
    fn reset(&self, table: &str, slot: &str) {
        self.exec(&[
            &format!(
                "SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots WHERE slot_name = '{slot}'"
            ),
            &format!("DROP PUBLICATION IF EXISTS {slot}_pub"),
            &format!("DROP TABLE IF EXISTS {table}"),
        ]);
    }

    fn rows(&self, file: &str) -> Vec<Value> {
        let out = std::process::Command::new(&self.bin)
            .args([":memory:", "-json", "-c", &format!("SELECT * FROM read_csv_auto('{file}')")])
            .output()
            .expect("duckdb runs");
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_default()
    }
}

fn node(id: &str, component: &str, props: Value) -> Value {
    json!({ "id": id, "position": { "x": 0, "y": 0 },
            "data": { "label": id, "componentId": component, "properties": props } })
}

fn doc(nodes: Value, edges: Value) -> PipelineDoc {
    serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).expect("doc")
}

fn edge(id: &str, s: &str, t: &str) -> Value {
    json!({ "id": id, "source": s, "target": t })
}

fn cdc(l: &Live, table: &str, slot: &str) -> Value {
    node("c", "src.postgres.cdc", json!({
        "connString": l.dsn, "table": format!("public.{table}"), "slotName": slot
    }))
}

/// The contract, end to end: nothing before the slot existed, each change
/// exactly once in commit order with its operation and typed values, nothing
/// again once delivered - and a run that FAILS delivers nothing, so the same
/// changes arrive on the next one instead of being lost.
#[test]
fn changes_arrive_once_in_order_and_survive_a_failed_run() {
    let l = live_or_skip!();
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (table, slot) = ("cdc_orders", "duckle_test_orders");
    l.reset(table, slot);
    l.exec(&[&format!(
        "CREATE TABLE {table}(id int PRIMARY KEY, note text, amount numeric(10,2), placed timestamptz, ok boolean)"
    )]);
    let ws = tempfile::tempdir().unwrap();
    std::env::set_var("DUCKLE_WORKSPACE", ws.path());
    let out = |n: u32| ws.path().join(format!("out{n}.csv")).to_string_lossy().replace('\\', "/");
    let run = |n: u32| {
        l.engine.execute_pipeline_named(
            &doc(
                json!([cdc(&l, table, slot), node("k", "snk.csv", json!({ "path": out(n), "hasHeader": true }))]),
                json!([edge("e1", "c", "k")]),
            ),
            "cdc_contract",
        )
    };

    // The first run creates the publication and slot. A slot begins where it
    // is made, so there is nothing to deliver yet.
    let r = run(1);
    assert_eq!(r.status, "ok", "first run: {:?}", r.error);
    assert_eq!(l.rows(&out(1)).len(), 0, "a new slot has no history");

    l.exec(&[
        &format!("INSERT INTO {table} VALUES (1, 'first', 9.50, '2026-01-02 03:04:05+00', true), (2, 'second', NULL, NULL, false)"),
        &format!("UPDATE {table} SET note = 'changed' WHERE id = 1"),
        &format!("DELETE FROM {table} WHERE id = 2"),
    ]);
    let r = run(2);
    assert_eq!(r.status, "ok", "second run: {:?}", r.error);
    let got = l.rows(&out(2));
    let ops: Vec<(String, i64)> = got
        .iter()
        .map(|v| (v["_op"].as_str().unwrap_or("?").to_string(), v["id"].as_i64().unwrap_or(-1)))
        .collect();
    assert_eq!(
        ops,
        vec![("insert".into(), 1), ("insert".into(), 2), ("update".into(), 1), ("delete".into(), 2)],
        "every change, in commit order: {got:?}"
    );
    assert_eq!(got[2]["note"], "changed", "an update carries the new row");
    assert_eq!(got[0]["amount"].to_string(), "9.5", "numeric arrives as a number: {got:?}");
    assert_eq!(got[0]["ok"], true, "boolean arrives as a boolean: {got:?}");
    assert!(got[0]["_lsn"].as_str().is_some_and(|s| s.contains('/')), "{got:?}");

    let r = run(3);
    assert_eq!(r.status, "ok", "third run: {:?}", r.error);
    assert_eq!(l.rows(&out(3)).len(), 0, "a delivered change is not delivered again");

    // A run that fails after reading must not move the position.
    l.exec(&[&format!("INSERT INTO {table} VALUES (3, 'third', 1, NULL, NULL)")]);
    let failing = l.engine.execute_pipeline_named(
        &doc(
            json!([
                cdc(&l, table, slot),
                node("die", "ctl.die", json!({ "message": "downstream broke", "condition": "always" })),
            ]),
            json!([edge("e1", "c", "die")]),
        ),
        "cdc_contract",
    );
    assert_ne!(failing.status, "ok", "the failing run should fail");
    let r = run(4);
    assert_eq!(r.status, "ok", "run after the failure: {:?}", r.error);
    let got = l.rows(&out(4));
    assert_eq!(got.len(), 1, "the change the failed run read is delivered again: {got:?}");
    assert_eq!(got[0]["id"], 3);

    l.reset(table, slot);
    std::env::remove_var("DUCKLE_WORKSPACE");
}

/// A large value an UPDATE did not touch is not sent. Taken from the old row
/// image under REPLICA IDENTITY FULL, and refused - naming the fix - without
/// it, because a NULL in its place would let a downstream upsert erase it.
#[test]
fn an_untouched_large_value_is_kept_or_refused_never_nulled() {
    let l = live_or_skip!();
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (table, slot) = ("cdc_docs", "duckle_test_docs");
    l.reset(table, slot);
    // md5 chains do not compress, so the body is stored out of line (TOAST).
    let big = "SELECT string_agg(md5(i::text), '') FROM generate_series(1, 400) i";
    l.exec(&[&format!("CREATE TABLE {table}(id int PRIMARY KEY, title text, body text)")]);
    let ws = tempfile::tempdir().unwrap();
    std::env::set_var("DUCKLE_WORKSPACE", ws.path());
    let out = ws.path().join("docs.csv").to_string_lossy().replace('\\', "/");
    let pipeline = doc(
        json!([cdc(&l, table, slot), node("k", "snk.csv", json!({ "path": out, "hasHeader": true }))]),
        json!([edge("e1", "c", "k")]),
    );
    let first = l.engine.execute_pipeline_named(&pipeline, "cdc_toast");
    assert_eq!(first.status, "ok", "slot setup: {:?}", first.error);

    l.exec(&[
        &format!("INSERT INTO {table} SELECT 1, 'a', ({big})"),
        &format!("UPDATE {table} SET title = 'b' WHERE id = 1"),
    ]);
    let refused = l.engine.execute_pipeline_named(&pipeline, "cdc_toast");
    assert_ne!(refused.status, "ok", "an unsent value must not become NULL");
    let err = refused.error.unwrap_or_default();
    assert!(err.contains("REPLICA IDENTITY FULL") && err.contains("body"), "{err}");

    // Under FULL the old image carries it, so it is filled in.
    l.reset(table, slot);
    l.exec(&[
        &format!("CREATE TABLE {table}(id int PRIMARY KEY, title text, body text)"),
        &format!("ALTER TABLE {table} REPLICA IDENTITY FULL"),
    ]);
    let ws2 = tempfile::tempdir().unwrap();
    std::env::set_var("DUCKLE_WORKSPACE", ws2.path());
    assert_eq!(l.engine.execute_pipeline_named(&pipeline, "cdc_toast").status, "ok");
    l.exec(&[
        &format!("INSERT INTO {table} SELECT 1, 'a', ({big})"),
        &format!("UPDATE {table} SET title = 'b' WHERE id = 1"),
    ]);
    let r = l.engine.execute_pipeline_named(&pipeline, "cdc_toast");
    assert_eq!(r.status, "ok", "{:?}", r.error);
    let got = l.rows(&out);
    let update = got.iter().find(|v| v["_op"] == "update").expect("the update arrived");
    assert_eq!(update["body"].as_str().map(str::len), Some(400 * 32), "the untouched body is whole");

    l.reset(table, slot);
    std::env::remove_var("DUCKLE_WORKSPACE");
}

/// A slot holds back WAL until it is consumed, so an idle or failing pipeline
/// grows the server's disk. The node says how far behind it is on every run,
/// and warns past the configured limit.
#[test]
fn the_node_reports_how_much_wal_its_slot_holds() {
    let l = live_or_skip!();
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (table, slot) = ("cdc_lag", "duckle_test_lag");
    l.reset(table, slot);
    l.exec(&[&format!("CREATE TABLE {table}(id int PRIMARY KEY)")]);
    let ws = tempfile::tempdir().unwrap();
    std::env::set_var("DUCKLE_WORKSPACE", ws.path());
    let mut source = cdc(&l, table, slot);
    source["data"]["properties"]["maxLagMb"] = json!(0);
    let pipeline = doc(json!([source]), json!([]));
    assert_eq!(l.engine.execute_pipeline_named(&pipeline, "cdc_lag").status, "ok");
    l.exec(&[&format!("INSERT INTO {table} SELECT generate_series(1, 1000)")]);
    let r = l.engine.execute_pipeline_named(&pipeline, "cdc_lag");
    assert_eq!(r.status, "ok", "a lag warning is not a failure: {:?}", r.error);
    let note = r.nodes.get("c").and_then(|n| n.note.clone()).unwrap_or_default();
    assert!(note.contains("holds") && note.contains("WAL"), "the lag is reported: {note}");
    assert!(note.contains("maxLagMb"), "and past the limit, says which limit: {note}");

    l.reset(table, slot);
    std::env::remove_var("DUCKLE_WORKSPACE");
}
