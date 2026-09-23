//! "From SQL": a pasted SELECT becomes a pipeline, one step per CTE.
//!
//! DuckDB parses the query (`json_serialize_sql`) and turns each part back into
//! SQL (`json_deserialize_sql`), so the steps are DuckDB's own reading of the
//! query rather than a second parser's. Each CTE becomes a SQL node whose SQL
//! name is the CTE's name, so the step after it reads it by the same name it
//! used in the query; the main SELECT becomes the last node. A bare table name
//! the query reads becomes a source placeholder to point at real data. A file
//! it reads (`FROM 'orders.csv'`) stays in the SQL, which already reads it.
//!
//! A `WITH RECURSIVE` cannot be split - a recursive step taken out on its own
//! is no longer recursive - so it stays one node, and the result says why.

use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, BTreeSet};

/// One step: a CTE, or the main SELECT (`name` None).
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub name: Option<String>,
    /// The part's own query node, as DuckDB serialized it.
    pub node: JsonValue,
    /// Names it reads: other CTEs, or base tables.
    pub reads: BTreeSet<String>,
}

/// What a serialized query splits into.
#[derive(Debug, Clone, PartialEq)]
pub struct Split {
    /// CTEs in the order they were written, then the main SELECT.
    pub parts: Vec<Part>,
    /// Base tables read anywhere, which become source placeholders.
    pub tables: BTreeSet<String>,
    /// Set when the query was kept as one node, with the reason.
    pub kept_whole: Option<String>,
}

/// Split the output of `json_serialize_sql` for one SELECT.
pub fn split(ast: &JsonValue) -> Result<Split, String> {
    if ast.get("error").and_then(JsonValue::as_bool) == Some(true) {
        let why = ast.get("error_message").and_then(JsonValue::as_str).unwrap_or("it did not parse");
        return Err(format!("the SQL could not be read: {why}"));
    }
    let statements = ast.get("statements").and_then(JsonValue::as_array).cloned().unwrap_or_default();
    let [statement] = statements.as_slice() else {
        return Err(format!(
            "paste one SELECT; this has {} statements",
            statements.len()
        ));
    };
    let root = statement.get("node").cloned().ok_or("the SQL has no query in it")?;
    let ctes: Vec<(String, JsonValue)> = root
        .pointer("/cte_map/map")
        .and_then(JsonValue::as_array)
        .map(|m| {
            m.iter()
                .filter_map(|e| {
                    Some((e.get("key")?.as_str()?.to_string(), e.pointer("/value/query/node")?.clone()))
                })
                .collect()
        })
        .unwrap_or_default();
    let cte_names: BTreeSet<String> = ctes.iter().map(|(n, _)| n.clone()).collect();

    // A recursive CTE is kept whole: split out, its step would stop recursing.
    if contains_type(&root, "RECURSIVE_CTE_NODE") {
        let mut reads = BTreeSet::new();
        collect_reads(&root, &mut reads);
        let tables: BTreeSet<String> = reads.difference(&cte_names).cloned().collect();
        return Ok(Split {
            parts: vec![Part { name: None, node: root, reads: tables.clone() }],
            tables,
            kept_whole: Some("it uses WITH RECURSIVE, which cannot be split into steps".into()),
        });
    }

    let mut parts = Vec::new();
    let mut tables = BTreeSet::new();
    for (name, node) in ctes {
        let mut reads = BTreeSet::new();
        collect_reads(&node, &mut reads);
        reads.remove(&name);
        tables.extend(reads.difference(&cte_names).cloned());
        parts.push(Part { name: Some(name), node, reads });
    }
    let mut main = root;
    if let Some(map) = main.pointer_mut("/cte_map/map") {
        *map = json!([]);
    }
    let mut reads = BTreeSet::new();
    collect_reads(&main, &mut reads);
    tables.extend(reads.difference(&cte_names).cloned());
    parts.push(Part { name: None, node: main, reads });
    Ok(Split { parts, tables, kept_whole: None })
}

/// Wrap a query node as a one-statement document `json_deserialize_sql` takes.
pub fn statement_json(node: &JsonValue) -> String {
    json!({ "error": false, "statements": [{ "node": node, "named_param_map": [] }] }).to_string()
}

/// Every bare table name a query reads, anywhere in it: joins, subqueries,
/// set operations. A name defined by a CTE nested inside it is not a table.
/// A quoted file path (`FROM 'orders.csv'`) is left to the SQL.
fn collect_reads(node: &JsonValue, out: &mut BTreeSet<String>) {
    let mut defined = BTreeSet::new();
    collect_nested_ctes(node, &mut defined, true);
    walk(node, &mut |n| {
        if n.get("type").and_then(JsonValue::as_str) == Some("BASE_TABLE") {
            if let Some(t) = n.get("table_name").and_then(JsonValue::as_str) {
                if !defined.contains(t) && !looks_like_a_file(t) {
                    out.insert(t.to_string());
                }
            }
        }
    });
}

/// CTE names defined inside a node, below its own top level.
fn collect_nested_ctes(node: &JsonValue, out: &mut BTreeSet<String>, top: bool) {
    match node {
        JsonValue::Object(map) => {
            if !top {
                if let Some(entries) = map.get("cte_map").and_then(|c| c.get("map")).and_then(JsonValue::as_array) {
                    out.extend(entries.iter().filter_map(|e| e.get("key")?.as_str().map(str::to_string)));
                }
            }
            for v in map.values() {
                collect_nested_ctes(v, out, false);
            }
        }
        JsonValue::Array(items) => items.iter().for_each(|v| collect_nested_ctes(v, out, false)),
        _ => {}
    }
}

fn walk(node: &JsonValue, f: &mut dyn FnMut(&JsonValue)) {
    f(node);
    match node {
        JsonValue::Object(map) => map.values().for_each(|v| walk(v, f)),
        JsonValue::Array(items) => items.iter().for_each(|v| walk(v, f)),
        _ => {}
    }
}

fn contains_type(node: &JsonValue, ty: &str) -> bool {
    let mut found = false;
    walk(node, &mut |n| found |= n.get("type").and_then(JsonValue::as_str) == Some(ty));
    found
}

fn looks_like_a_file(name: &str) -> bool {
    name.contains('/') || name.contains('\\') || name.contains("://") || {
        let lower = name.to_ascii_lowercase();
        [".csv", ".tsv", ".parquet", ".json", ".jsonl", ".ndjson", ".xlsx"].iter().any(|e| lower.ends_with(e))
    }
}

/// Build the pipeline document from a split and each part's SQL text.
///
/// Sources on the left, then each step one column further right than the
/// furthest step it reads, so the canvas reads in the order the query runs.
pub fn pipeline(split: &Split, sql_of: &[String]) -> JsonValue {
    let node_id = |name: &str| format!("step_{}", sanitize(name));
    let source_id = |name: &str| format!("src_{}", sanitize(name));
    let mut depth: BTreeMap<String, usize> = split.tables.iter().map(|t| (t.clone(), 0)).collect();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut per_column: BTreeMap<usize, usize> = BTreeMap::new();
    let mut place = |col: usize| {
        let row = per_column.entry(col).or_insert(0);
        let pos = json!({ "x": 60 + col * 300, "y": 60 + *row * 160 });
        *row += 1;
        pos
    };
    for table in &split.tables {
        nodes.push(json!({
            "id": source_id(table),
            "type": "source",
            "position": place(0),
            "data": {
                "label": format!("{table} (set the source)"),
                "componentId": "src.duckdb",
                "alias": table,
                "properties": { "tableName": table }
            }
        }));
    }
    for (i, part) in split.parts.iter().enumerate() {
        let (id, label) = match &part.name {
            Some(n) => (node_id(n), n.clone()),
            None => ("result".to_string(), "Result".to_string()),
        };
        let col = part.reads.iter().filter_map(|r| depth.get(r)).max().map_or(1, |d| d + 1);
        if let Some(n) = &part.name {
            depth.insert(n.clone(), col);
        }
        let mut data = json!({
            "label": label,
            "componentId": "code.sql",
            "properties": { "sql": sql_of.get(i).cloned().unwrap_or_default(), "rawSql": true }
        });
        if let Some(n) = &part.name {
            data["alias"] = json!(n);
        }
        // Said on the node itself, where the person looking at one big step
        // will wonder why it was not split.
        if let (None, Some(why)) = (&part.name, &split.kept_whole) {
            data["subtitle"] = json!(format!("Kept as one step: {why}"));
        }
        nodes.push(json!({ "id": id, "type": "transform", "position": place(col), "data": data }));
        // A SQL node has one data input, and the engine refuses a second rather
        // than drop it. The step reads everything by name, so one edge carries
        // the main flow - from the nearest earlier step it reads, else a table -
        // and the rest only say "after this", which orders without wiring.
        let main_read = split
            .parts
            .iter()
            .rev()
            .filter_map(|p| p.name.as_ref())
            .find(|n| part.reads.contains(*n))
            .or_else(|| part.reads.iter().next());
        for read in &part.reads {
            let from = if split.tables.contains(read) { source_id(read) } else { node_id(read) };
            // Drawn as the canvas draws its own: a `duckle` edge whose
            // connectionType says whether rows flow along it.
            let kind = if Some(read) == main_read { "main" } else { "on-component-ok" };
            edges.push(json!({
                "id": format!("e_{}_{}", from, id),
                "source": from,
                "target": id,
                "sourceHandle": "main",
                "targetHandle": "main",
                "type": "duckle",
                "data": { "connectionType": kind }
            }));
        }
    }
    json!({ "nodes": nodes, "edges": edges })
}

fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(t: &str) -> JsonValue {
        json!({ "type": "BASE_TABLE", "table_name": t })
    }

    /// A query shaped as DuckDB serializes one: two CTEs, the second reading
    /// the first, and a main SELECT joining the second with a table.
    fn ast() -> JsonValue {
        json!({ "error": false, "statements": [{ "node": {
            "type": "SELECT_NODE",
            "cte_map": { "map": [
                { "key": "paid", "value": { "query": { "node": { "type": "SELECT_NODE", "from_table": base("orders") } } } },
                { "key": "totals", "value": { "query": { "node": { "type": "SELECT_NODE", "from_table": base("paid") } } } }
            ] },
            "from_table": { "type": "JOIN", "left": base("totals"), "right": base("customers") }
        } }] })
    }

    #[test]
    fn each_cte_is_a_step_reading_what_it_read() {
        let s = split(&ast()).unwrap();
        let names: Vec<Option<&str>> = s.parts.iter().map(|p| p.name.as_deref()).collect();
        assert_eq!(names, vec![Some("paid"), Some("totals"), None]);
        let reads = |i: usize| s.parts[i].reads.iter().cloned().collect::<Vec<_>>();
        assert_eq!(reads(0), vec!["orders"]);
        assert_eq!(reads(1), vec!["paid"]);
        assert_eq!(reads(2), vec!["customers", "totals"]);
        assert_eq!(s.tables.iter().cloned().collect::<Vec<_>>(), vec!["customers", "orders"]);
        assert!(s.kept_whole.is_none());
    }

    #[test]
    fn the_graph_wires_steps_by_what_they_read() {
        let s = split(&ast()).unwrap();
        let doc = pipeline(&s, &["a".into(), "b".into(), "c".into()]);
        let edges: BTreeSet<(String, String)> = doc["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["source"].as_str().unwrap().to_string(), e["target"].as_str().unwrap().to_string()))
            .collect();
        let want: BTreeSet<(String, String)> = [
            ("src_orders", "step_paid"),
            ("step_paid", "step_totals"),
            ("step_totals", "result"),
            ("src_customers", "result"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        assert_eq!(edges, want);
        // One data edge into each step - the engine refuses a second - and it
        // follows the chain of steps rather than the side table.
        let data_into_result: Vec<&str> = doc["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["target"] == "result" && e["data"]["connectionType"] == "main")
            .map(|e| e["source"].as_str().unwrap())
            .collect();
        assert_eq!(data_into_result, vec!["step_totals"]);
        // A step is named for the CTE it was, so the SQL after it reads it unchanged.
        let paid = doc["nodes"].as_array().unwrap().iter().find(|n| n["id"] == "step_paid").unwrap();
        assert_eq!(paid["data"]["alias"], "paid");
    }

    #[test]
    fn a_file_the_query_reads_is_left_to_the_sql() {
        let ast = json!({ "error": false, "statements": [{ "node": {
            "type": "SELECT_NODE", "cte_map": { "map": [] }, "from_table": base("data/orders.csv")
        } }] });
        assert!(split(&ast).unwrap().tables.is_empty());
    }

    #[test]
    fn a_recursive_query_is_kept_whole_and_says_why() {
        let ast = json!({ "error": false, "statements": [{ "node": {
            "type": "SELECT_NODE",
            "cte_map": { "map": [{ "key": "t", "value": { "query": { "node": { "type": "RECURSIVE_CTE_NODE" } } } }] },
            "from_table": base("t")
        } }] });
        let s = split(&ast).unwrap();
        assert_eq!(s.parts.len(), 1);
        assert!(s.kept_whole.unwrap().contains("RECURSIVE"));
    }

    #[test]
    fn more_than_one_statement_or_a_parse_error_is_refused() {
        let two = json!({ "error": false, "statements": [{ "node": {} }, { "node": {} }] });
        assert!(split(&two).unwrap_err().contains("one SELECT"));
        let bad = json!({ "error": true, "error_message": "syntax error at or near FORM" });
        assert!(split(&bad).unwrap_err().contains("FORM"));
    }
}
