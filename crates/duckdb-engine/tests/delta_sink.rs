//! `snk.delta` against real Delta tables, read back through DuckDB's own
//! `delta_scan` - the delta-kernel reader - rather than through the sink.

use duckle_duckdb_engine::{DuckdbEngine, PipelineDoc};
use serde_json::{json, Value};
use std::path::Path;

fn engine() -> Option<(DuckdbEngine, String)> {
    let bin = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists())?;
    Some((DuckdbEngine::new(bin.clone().into()), bin))
}

macro_rules! engine_or_skip {
    () => {
        match engine() {
            Some(e) => e,
            None => {
                eprintln!("skipping: set DUCKLE_DUCKDB_BIN to a duckdb CLI to run");
                return;
            }
        }
    };
}

fn node(id: &str, component: &str, props: Value) -> Value {
    json!({ "id": id, "position": { "x": 0, "y": 0 },
            "data": { "label": id, "componentId": component, "properties": props } })
}

fn pipeline(csv: &str, table: &str, extra: Value) -> PipelineDoc {
    let mut props = json!({ "path": table });
    if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
        p.extend(e.clone());
    }
    serde_json::from_value(json!({
        "nodes": [
            node("s", "src.csv", json!({ "path": csv, "hasHeader": true })),
            node("k", "snk.delta", props),
        ],
        "edges": [{ "id": "e", "source": "s", "target": "k" }]
    }))
    .expect("doc")
}

fn path(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().replace('\\', "/")
}

fn write(dir: &Path, name: &str, body: &str) -> String {
    std::fs::write(dir.join(name), body).unwrap();
    path(dir, name)
}

/// Query a Delta table through delta-kernel, independent of the sink.
fn read(bin: &str, sql: &str) -> Vec<Value> {
    let out = std::process::Command::new(bin)
        .args([":memory:", "-json", "-c", &format!("LOAD delta; {sql}")])
        .output()
        .expect("duckdb runs");
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_default()
}

/// A missing table is created from the input's schema, and later runs append.
/// DuckDB's delta extension can append but cannot create a table, so the sink
/// writes the first commit itself; that it is a real Delta table is proved by
/// the kernel reading it back, typed.
#[test]
fn a_delta_sink_creates_the_table_then_appends_to_it() {
    let (engine, bin) = engine_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    let csv = write(
        tmp.path(),
        "in.csv",
        "id,name,amount,seen\n1,a,9.50,2026-01-02 03:04:05\n2,b,1.25,2026-02-03 04:05:06\n",
    );
    let table = path(tmp.path(), "orders");
    for run in 1..=2 {
        let r = engine.execute_pipeline(&pipeline(&csv, &table, json!({})));
        assert_eq!(r.status, "ok", "run {run}: {:?}", r.error);
        assert_eq!(r.nodes.get("k").and_then(|n| n.rows), Some(2), "run {run} reports what it wrote");
    }
    let got = read(&bin, &format!("SELECT count(*) AS n FROM delta_scan('{table}')"));
    assert_eq!(got[0]["n"], 4, "created on the first run, appended on the second");
    let types = read(&bin, &format!("DESCRIBE SELECT * FROM delta_scan('{table}')"));
    let t: Vec<(&str, &str)> = types
        .iter()
        .map(|c| (c["column_name"].as_str().unwrap_or(""), c["column_type"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(
        t,
        vec![("id", "BIGINT"), ("name", "VARCHAR"), ("amount", "DOUBLE"), ("seen", "TIMESTAMP")],
        "each column keeps its type, a naive timestamp included"
    );
    assert!(
        Path::new(&table).join("_delta_log").join("00000000000000000002.json").exists(),
        "one commit to create and one per append"
    );
}

/// A column the table does not have is refused by name, never dropped.
#[test]
fn a_column_the_table_lacks_is_refused_by_name() {
    let (engine, _) = engine_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    let table = path(tmp.path(), "t");
    let first = write(tmp.path(), "a.csv", "id\n1\n");
    assert_eq!(engine.execute_pipeline(&pipeline(&first, &table, json!({}))).status, "ok");
    let wider = write(tmp.path(), "b.csv", "id,surprise\n2,x\n");
    let r = engine.execute_pipeline(&pipeline(&wider, &table, json!({})));
    assert_ne!(r.status, "ok", "a column with nowhere to go must not vanish");
    let err = r.error.unwrap_or_default();
    assert!(err.contains("the table has no column surprise"), "{err}");
}

/// A column the input does not supply is refused by name, never NULL-filled.
#[test]
fn a_column_the_input_lacks_is_refused_by_name() {
    let (engine, _) = engine_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    let table = path(tmp.path(), "t");
    let first = write(tmp.path(), "a.csv", "id,note\n1,x\n");
    assert_eq!(engine.execute_pipeline(&pipeline(&first, &table, json!({}))).status, "ok");
    let narrower = write(tmp.path(), "b.csv", "id\n2\n");
    let r = engine.execute_pipeline(&pipeline(&narrower, &table, json!({})));
    assert_ne!(r.status, "ok");
    // The sink's own refusal, not DuckDB's binder happening to name the column.
    let err = r.error.unwrap_or_default();
    assert!(err.contains("the input has no column note"), "{err}");
}

/// Columns in a different order land in the right place. The extension's own
/// `INSERT ... BY NAME` fails an internal assertion, so the sink orders them.
#[test]
fn columns_are_matched_by_name_not_position() {
    let (engine, bin) = engine_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    let table = path(tmp.path(), "t");
    let first = write(tmp.path(), "a.csv", "id,note\n1,x\n");
    assert_eq!(engine.execute_pipeline(&pipeline(&first, &table, json!({}))).status, "ok");
    let reordered = write(tmp.path(), "b.csv", "note,id\ny,2\n");
    let r = engine.execute_pipeline(&pipeline(&reordered, &table, json!({})));
    assert_eq!(r.status, "ok", "{:?}", r.error);
    let got = read(&bin, &format!("SELECT note FROM delta_scan('{table}') WHERE id = 2"));
    assert_eq!(got[0]["note"], "y");
}

/// Without createIfMissing, a missing table is an error rather than a new one
/// somewhere a typo put it.
#[test]
fn a_missing_table_is_refused_when_creating_is_off() {
    let (engine, _) = engine_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    let csv = write(tmp.path(), "a.csv", "id\n1\n");
    let table = path(tmp.path(), "not_there");
    let r = engine.execute_pipeline(&pipeline(&csv, &table, json!({ "createIfMissing": false })));
    assert_ne!(r.status, "ok");
    assert!(!Path::new(&table).exists(), "nothing was created");
}

/// A type Delta has no column type for is refused at create, naming the column,
/// rather than written as something else.
#[test]
fn a_type_delta_cannot_hold_is_refused_at_create() {
    let (engine, _) = engine_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    let csv = write(tmp.path(), "a.csv", "id\n1\n");
    let table = path(tmp.path(), "t");
    let doc: PipelineDoc = serde_json::from_value(json!({
        "nodes": [
            node("s", "src.csv", json!({ "path": csv, "hasHeader": true })),
            node("x", "code.sql", json!({ "sql": "SELECT id, INTERVAL 1 DAY AS wait FROM input" })),
            node("k", "snk.delta", json!({ "path": table })),
        ],
        "edges": [
            { "id": "e1", "source": "s", "target": "x" },
            { "id": "e2", "source": "x", "target": "k" }
        ]
    }))
    .unwrap();
    let r = engine.execute_pipeline(&doc);
    assert_ne!(r.status, "ok");
    let err = r.error.unwrap_or_default();
    assert!(err.contains("wait") && err.contains("INTERVAL"), "{err}");
    assert!(!Path::new(&table).join("_delta_log").exists(), "no half-made table is left");
}

/// Remote tables are refused until a remote write has been tested.
#[test]
fn a_remote_path_is_refused_for_now() {
    let (engine, _) = engine_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    let csv = write(tmp.path(), "a.csv", "id\n1\n");
    let r = engine.execute_pipeline(&pipeline(&csv, "s3://bucket/orders", json!({})));
    assert_ne!(r.status, "ok");
    assert!(r.error.unwrap_or_default().contains("local"));
}
