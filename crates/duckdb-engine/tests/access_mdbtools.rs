//! src.access where the Access ODBC driver does not exist: mdbtools.
//!
//! Linux and macOS, and only where mdbtools is installed. Nothing here can
//! write an Access file, so the two fixtures beside this file were made by
//! Access itself (ADOX, on Windows) and hold the same rows: an .accdb and an
//! .mdb, with accented text, a code with leading zeros, a quote and a comma
//! in a memo, currency, a 20-digit decimal, yes/no and a date/time.
#![cfg(not(windows))]

use duckle_duckdb_engine::{DuckdbEngine, PipelineDoc};
use serde_json::{json, Value};
use std::path::Path;

fn ready() -> Option<(DuckdbEngine, String)> {
    let bin = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists())?;
    let mdbtools = std::process::Command::new("mdb-export")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !mdbtools {
        eprintln!("skipping: mdbtools is not installed");
        return None;
    }
    Some((DuckdbEngine::new(bin.clone().into()), bin))
}

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(name).to_string_lossy().into_owned()
}

fn doc(nodes: Value, edges: Value) -> PipelineDoc {
    serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).unwrap()
}

fn node(id: &str, component: &str, props: Value) -> Value {
    json!({ "id": id, "position": { "x": 0, "y": 0 },
            "data": { "label": id, "componentId": component, "properties": props } })
}

fn query(bin: &str, sql: &str) -> Vec<Value> {
    let out = std::process::Command::new(bin)
        .args([":memory:", "-json", "-c", sql])
        .output()
        .expect("duckdb runs");
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_default()
}

/// Both file formats read with their types and their text as Access holds
/// them.
#[test]
fn a_table_reads_through_mdbtools_with_its_types() {
    let Some((engine, bin)) = ready() else { return };
    for file in ["fixture.accdb", "fixture.mdb"] {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out.parquet").to_string_lossy().into_owned();
        let r = engine.execute_pipeline(&doc(
            json!([
                node("a", "src.access", json!({ "path": fixture(file), "tableName": "People" })),
                node("k", "snk.parquet", json!({ "path": out })),
            ]),
            json!([{ "id": "e", "source": "a", "target": "k" }]),
        ));
        assert_eq!(r.status, "ok", "{file}: {:?}", r.error);

        let rows = query(
            &bin,
            &format!(
                "SELECT ID, Name, Code, Qty, CAST(Amount AS VARCHAR) AS Amount, Active, \
                 CAST(Joined AS VARCHAR) AS Joined, Notes, CAST(Big AS VARCHAR) AS Big \
                 FROM '{out}' ORDER BY ID"
            ),
        );
        assert_eq!(rows.len(), 3, "{file}: {rows:?}");
        assert_eq!(rows[0]["Name"], "Zoë Müller", "{file}");
        assert_eq!(rows[0]["Code"], "007", "{file}: a code keeps its leading zeros");
        assert_eq!(rows[0]["Qty"], 3, "{file}");
        assert_eq!(rows[0]["Amount"], "12.3400", "{file}: currency is exact");
        assert_eq!(rows[0]["Active"], true, "{file}");
        assert_eq!(rows[1]["Active"], false, "{file}");
        assert_eq!(rows[0]["Joined"], "2026-09-24 10:11:12", "{file}: a four-digit year");
        assert_eq!(rows[0]["Notes"], "has a \"quote\" and, a comma", "{file}");
        assert_eq!(rows[0]["Big"], "1234567890123456.1234", "{file}: all twenty digits");
        assert!(rows[1]["Notes"].is_null(), "{file}");
        // mdbtools reads a zero-length text as NULL; there is no telling the
        // two apart from its output, which is what the README says.
        assert!(rows[2]["Name"].is_null(), "{file}");

        let types: std::collections::HashMap<String, String> = query(
            &bin,
            &format!("SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM '{out}')"),
        )
        .into_iter()
        .map(|r| (r["column_name"].as_str().unwrap().into(), r["column_type"].as_str().unwrap().into()))
        .collect();
        assert_eq!(types["ID"], "INTEGER", "{file}: {types:?}");
        assert_eq!(types["Price"], "DOUBLE", "{file}: {types:?}");
        assert_eq!(types["Amount"], "DECIMAL(19,4)", "{file}: {types:?}");
        assert_eq!(types["Active"], "BOOLEAN", "{file}: {types:?}");
        assert_eq!(types["Joined"], "TIMESTAMP", "{file}: {types:?}");
        assert_eq!(types["Big"], "DECIMAL(20,4)", "{file}: {types:?}");
        assert_eq!(types["Code"], "VARCHAR", "{file}: {types:?}");
    }
}

/// A query needs the Windows driver, and a write needs Windows: both say so
/// rather than doing something else.
#[test]
fn what_needs_windows_says_so() {
    let Some((engine, _)) = ready() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out.parquet").to_string_lossy().into_owned();
    let read = engine.execute_pipeline(&doc(
        json!([
            node("a", "src.access", json!({ "path": fixture("fixture.accdb"), "query": "SELECT * FROM [People]" })),
            node("k", "snk.parquet", json!({ "path": out })),
        ]),
        json!([{ "id": "e", "source": "a", "target": "k" }]),
    ));
    assert_ne!(read.status, "ok");
    assert!(format!("{:?}", read.error).contains("Windows"), "{:?}", read.error);

    let write = engine.execute_pipeline(&doc(
        json!([
            node("s", "code.sql", json!({ "sql": "SELECT 1 AS id" })),
            node("w", "snk.access", json!({ "path": tmp.path().join("x.accdb").to_string_lossy(), "tableName": "T" })),
        ]),
        json!([{ "id": "e", "source": "s", "target": "w" }]),
    ));
    assert_ne!(write.status, "ok");
    assert!(format!("{:?}", write.error).contains("Windows"), "{:?}", write.error);
}
