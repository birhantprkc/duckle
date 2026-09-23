//! A cloud read with no key in the pipeline: `cloudAuth: environment`.
//!
//! Its own test binary on purpose. The credentials are put in this process's
//! environment, which the DuckDB child inherits, and a file of its own keeps
//! them away from every other test in the suite.

use duckle_duckdb_engine::{DuckdbEngine, PipelineDoc};
use serde_json::{json, Value};
use std::path::Path;

fn doc(nodes: Value, edges: Value) -> PipelineDoc {
    serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).unwrap()
}

/// The same node, run twice: without an identity in the environment it
/// cannot read, and with one it reads the object. Nothing in the pipeline
/// changes between the two, so the key can only have come from where the run
/// was. MinIO stands in for S3: it speaks the same signing, and the AWS chain
/// cannot tell the two apart.
#[test]
fn an_s3_read_takes_its_credentials_from_where_it_runs() {
    let Some(bin) = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists()) else {
        eprintln!("skipping: set DUCKLE_DUCKDB_BIN to run");
        return;
    };
    let host = match std::env::var("DUCKLE_MINIO_HOST") {
        Ok(h) if !h.is_empty() => h,
        _ => {
            eprintln!("skipping: set DUCKLE_MINIO_HOST to run against MinIO");
            return;
        }
    };
    let port = std::env::var("DUCKLE_MINIO_PORT").unwrap_or_else(|_| "9000".into());
    let bucket = std::env::var("DUCKLE_MINIO_BUCKET").unwrap_or_else(|_| "duckle-test".into());
    let access = std::env::var("DUCKLE_MINIO_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into());
    let secret = std::env::var("DUCKLE_MINIO_SECRET_KEY").unwrap_or_else(|_| "minioadmin".into());
    let engine = DuckdbEngine::new(bin.clone().into());

    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out.csv").to_string_lossy().replace('\\', "/");
    let pipeline = || {
        doc(
            json!([
                { "id": "r", "position": { "x": 0, "y": 0 }, "data": { "label": "r", "componentId": "src.minio",
                  "properties": {
                    "bucket": bucket, "key": "orders.parquet", "format": "parquet",
                    "cloudAuth": "environment", "region": "us-east-1",
                    "endpoint": format!("{host}:{port}"), "urlStyle": "path", "useSsl": "false"
                  } } },
                { "id": "k", "position": { "x": 0, "y": 0 }, "data": { "label": "k", "componentId": "snk.csv",
                  "properties": { "path": out, "hasHeader": true } } }
            ]),
            json!([{ "id": "e", "source": "r", "target": "k" }]),
        )
    };

    // No identity here: the pipeline has no key of its own to fall back on.
    for k in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_PROFILE"] {
        std::env::remove_var(k);
    }
    let without = engine.execute_pipeline(&pipeline());
    assert_ne!(without.status, "ok", "read with no identity anywhere: {:?}", without.error);

    std::env::set_var("AWS_ACCESS_KEY_ID", &access);
    std::env::set_var("AWS_SECRET_ACCESS_KEY", &secret);
    let with = engine.execute_pipeline(&pipeline());
    assert_eq!(with.status, "ok", "the environment's identity was not used: {:?}", with.error);

    let rows = std::process::Command::new(&bin)
        .args([":memory:", "-noheader", "-list", "-c", &format!("SELECT count(*) FROM read_csv('{out}')")])
        .output()
        .expect("duckdb runs");
    assert_eq!(String::from_utf8_lossy(&rows.stdout).trim(), "3", "the seeded object's rows");
}
