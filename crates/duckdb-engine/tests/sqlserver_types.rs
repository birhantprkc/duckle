//! #362: every SQL Server date/time type reads as its DuckDB type.
//!
//! Needs a SQL Server: DUCKLE_MSSQL_HOST (and _PORT, _USER, _PASS), the same
//! variables tests/execution.rs uses. No table is needed - the query casts
//! literals, which is also the reporter's "explicit cast" case.

use duckle_duckdb_engine::{DuckdbEngine, PipelineDoc};
use serde_json::{json, Value};
use std::path::Path;

fn setup() -> Option<(DuckdbEngine, String, Value)> {
    let bin = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists())?;
    let Ok(host) = std::env::var("DUCKLE_MSSQL_HOST") else {
        eprintln!("skipping: set DUCKLE_MSSQL_HOST to run against SQL Server");
        return None;
    };
    let port: u64 = std::env::var("DUCKLE_MSSQL_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(1433);
    let props = json!({
        "host": host, "port": port, "database": "master",
        "user": std::env::var("DUCKLE_MSSQL_USER").unwrap_or_else(|_| "sa".into()),
        "password": std::env::var("DUCKLE_MSSQL_PASS").unwrap_or_default(),
        "trustCert": true,
    });
    Some((DuckdbEngine::new(bin.clone().into()), bin, props))
}

fn query(bin: &str, sql: &str) -> Vec<Value> {
    let out = std::process::Command::new(bin)
        .args([":memory:", "-json", "-c", sql])
        .output()
        .expect("duckdb runs");
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_default()
}

const CASTS: &str = "\
    SELECT 1 AS id, \
      CAST('2026-09-24T10:11:12.123' AS datetime) AS dt, \
      CAST('2026-09-24T10:11:12.1234567' AS datetime2(7)) AS dt2, \
      CAST('2026-09-24T10:11:12' AS datetime2(0)) AS dt2_whole, \
      CAST('2026-09-24T10:11:00' AS smalldatetime) AS sdt, \
      CAST('2026-09-24' AS date) AS d, \
      CAST('10:11:12.1234567' AS time(7)) AS t, \
      CAST('2026-09-24T10:11:12.5+02:00' AS datetimeoffset(7)) AS dto \
    UNION ALL SELECT 2, \
      CAST('1999-12-31T23:59:59' AS datetime), \
      CAST('1999-12-31T23:59:59' AS datetime2(7)), \
      NULL, NULL, NULL, NULL, NULL";

/// Each column is typed from what SQL Server says it is, not guessed from its
/// text: a datetime with milliseconds, a datetime2 with seven digits of
/// fraction, a whole-second datetime2, a smalldatetime, a date, a time and a
/// datetimeoffset all come back as dates and times.
#[test]
fn every_sql_server_date_and_time_type_reads_as_one() {
    let Some((engine, bin, mut props)) = setup() else { return };
    props["query"] = json!(CASTS);
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out.parquet").to_string_lossy().replace('\\', "/");
    let doc: PipelineDoc = serde_json::from_value(json!({
        "nodes": [
            { "id": "s", "position": { "x": 0, "y": 0 }, "data": { "label": "s", "componentId": "src.sqlserver", "properties": props } },
            { "id": "k", "position": { "x": 0, "y": 0 }, "data": { "label": "k", "componentId": "snk.parquet", "properties": { "path": out } } }
        ],
        "edges": [{ "id": "e", "source": "s", "target": "k" }]
    }))
    .unwrap();
    let r = engine.execute_pipeline(&doc);
    assert_eq!(r.status, "ok", "{:?}", r.error);

    let types: std::collections::BTreeMap<String, String> =
        query(&bin, &format!("SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM '{out}')"))
            .into_iter()
            .map(|c| (c["column_name"].as_str().unwrap().into(), c["column_type"].as_str().unwrap().into()))
            .collect();
    for (col, want) in [
        ("dt", "TIMESTAMP"),
        ("dt2", "TIMESTAMP"),
        ("dt2_whole", "TIMESTAMP"),
        ("sdt", "TIMESTAMP"),
        ("d", "DATE"),
        ("t", "TIME"),
        ("dto", "TIMESTAMP WITH TIME ZONE"),
    ] {
        assert_eq!(types.get(col).map(String::as_str), Some(want), "{col}: {types:?}");
    }

    let rows = query(
        &bin,
        &format!(
            "SELECT id, CAST(dt AS VARCHAR) dt, CAST(dt2 AS VARCHAR) dt2, CAST(sdt AS VARCHAR) sdt, \
             CAST(t AS VARCHAR) t, CAST(dto AT TIME ZONE 'UTC' AS VARCHAR) dto FROM '{out}' ORDER BY id"
        ),
    );
    assert_eq!(rows[0]["dt"], "2026-09-24 10:11:12.123", "{rows:?}");
    // DuckDB keeps microseconds, so the seventh digit is the only thing lost.
    assert_eq!(rows[0]["dt2"], "2026-09-24 10:11:12.123456", "{rows:?}");
    assert_eq!(rows[0]["sdt"], "2026-09-24 10:11:00", "{rows:?}");
    assert_eq!(rows[0]["t"], "10:11:12.123456", "{rows:?}");
    assert_eq!(rows[0]["dto"], "2026-09-24 08:11:12.5", "the instant, offset applied: {rows:?}");
    assert_eq!(rows[1]["dt"], "1999-12-31 23:59:59", "{rows:?}");
    assert!(rows[1]["sdt"].is_null(), "{rows:?}");
}
