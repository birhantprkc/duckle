//! #364: a ClickHouse table read through ClickHouse's ADBC driver shows its
//! values as they are, not as the numbers and bytes ClickHouse's Arrow export
//! flattens them to.
//!
//! Needs the driver and a server: DUCKLE_CLICKHOUSE_ADBC_DRIVER (the driver
//! library, e.g. from `dbc install clickhouse`) and DUCKLE_CLICKHOUSE_URL
//! (http://localhost:8123/). The table is made over ClickHouse's HTTP
//! interface directly, so the setup does not go through the code under test.

use duckle_duckdb_engine::{DuckdbEngine, PipelineDoc};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::Path;

fn clickhouse(url: &str, sql: &str) -> String {
    let host = url.trim_start_matches("http://").trim_end_matches('/').split('/').next().unwrap().to_string();
    let mut s = std::net::TcpStream::connect(&host).expect("ClickHouse is reachable");
    write!(
        s,
        "POST / HTTP/1.0\r\nHost: {host}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\r\n{sql}",
        sql.len()
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    assert!(head.starts_with("HTTP/1.0 200") || head.starts_with("HTTP/1.1 200"), "{sql}: {raw}");
    body.to_string()
}

fn rows(bin: &str, sql: &str) -> Vec<Value> {
    let out = std::process::Command::new(bin)
        .args([":memory:", "-json", "-c", sql])
        .output()
        .expect("duckdb runs");
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_default()
}

#[test]
fn clickhouse_types_arrive_as_what_they_are() {
    let Some(bin) = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists()) else { return };
    let (Ok(driver), Ok(url)) =
        (std::env::var("DUCKLE_CLICKHOUSE_ADBC_DRIVER"), std::env::var("DUCKLE_CLICKHOUSE_URL"))
    else {
        eprintln!("skipping: set DUCKLE_CLICKHOUSE_ADBC_DRIVER and DUCKLE_CLICKHOUSE_URL");
        return;
    };
    let table = format!("duckle_364_{}", std::process::id());
    clickhouse(&url, &format!("DROP TABLE IF EXISTS {table}"));
    clickhouse(
        &url,
        &format!(
            "CREATE TABLE {table} (id UInt8, name String, dt DateTime, dtn Nullable(DateTime), \
             dt_tz DateTime('Asia/Kolkata'), ts3 DateTime64(3), u UUID, e Enum8('a' = 1, 'b' = 2), \
             ip IPv4, fs FixedString(3), i128 Int128, u256 UInt256) ENGINE = MergeTree ORDER BY id"
        ),
    );
    clickhouse(
        &url,
        &format!(
            "INSERT INTO {table} VALUES (1, 'Zoë', '2026-09-24 10:11:12', NULL, '2026-09-24 10:11:12', \
             '2026-09-24 10:11:12.345', '61f0c404-5cb3-11e7-907b-a6006ad3dba0', 'b', '192.168.1.10', 'abc', \
             -170141183460469231731687303715884105728, 1234567890123456789012345678901234567890)"
        ),
    );

    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out.parquet").to_string_lossy().replace('\\', "/");
    let doc: PipelineDoc = serde_json::from_value(json!({
        "nodes": [
            { "id": "ch", "position": { "x": 0, "y": 0 }, "data": { "label": "ch", "componentId": "src.adbc",
              "properties": { "driver": driver, "entrypoint": "AdbcClickhouseInit", "uri": url,
                              "query": format!("SELECT * FROM {table} LIMIT 5;") } } },
            { "id": "k", "position": { "x": 0, "y": 0 }, "data": { "label": "k", "componentId": "snk.parquet",
              "properties": { "path": out } } }
        ],
        "edges": [{ "id": "e", "source": "ch", "target": "k" }]
    }))
    .unwrap();
    let r = DuckdbEngine::new(bin.clone().into()).execute_pipeline(&doc);
    clickhouse(&url, &format!("DROP TABLE IF EXISTS {table}"));
    assert_eq!(r.status, "ok", "{:?}", r.error);

    let types: std::collections::BTreeMap<String, String> =
        rows(&bin, &format!("SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM '{out}')"))
            .into_iter()
            .map(|c| (c["column_name"].as_str().unwrap().into(), c["column_type"].as_str().unwrap().into()))
            .collect();
    for (col, want) in [
        ("dt", "TIMESTAMP WITH TIME ZONE"),
        ("dtn", "TIMESTAMP WITH TIME ZONE"),
        ("dt_tz", "TIMESTAMP WITH TIME ZONE"),
        ("ts3", "TIMESTAMP WITH TIME ZONE"),
        ("u", "VARCHAR"),
        ("e", "VARCHAR"),
        ("ip", "VARCHAR"),
        ("fs", "VARCHAR"),
        ("i128", "VARCHAR"),
        ("u256", "VARCHAR"),
        ("name", "VARCHAR"),
    ] {
        assert_eq!(types.get(col).map(String::as_str), Some(want), "{col}: {types:?}");
    }

    let got = rows(
        &bin,
        &format!(
            "SELECT name, strftime(dt AT TIME ZONE 'UTC', '%Y-%m-%d %H:%M:%S') AS dt, dtn, \
             strftime(dt_tz AT TIME ZONE 'UTC', '%Y-%m-%d %H:%M:%S') AS dt_tz, u, e, ip, fs, i128, u256 FROM '{out}'"
        ),
    );
    assert_eq!(
        got,
        vec![json!({
            "name": "Zoë",
            "dt": "2026-09-24 10:11:12",
            "dtn": null,
            // 10:11:12 in Kolkata is 04:41:12 UTC: the column's zone is kept.
            "dt_tz": "2026-09-24 04:41:12",
            "u": "61f0c404-5cb3-11e7-907b-a6006ad3dba0",
            "e": "b",
            "ip": "192.168.1.10",
            "fs": "abc",
            "i128": "-170141183460469231731687303715884105728",
            "u256": "1234567890123456789012345678901234567890"
        })]
    );
}
