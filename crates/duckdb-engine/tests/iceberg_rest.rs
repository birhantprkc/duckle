//! Iceberg REST catalogs, against a real catalog and object store.
//!
//! Needs DUCKLE_DUCKDB_BIN and a REST catalog whose warehouse lives on S3-
//! compatible storage, named by DUCKLE_ICEBERG_REST_URI, DUCKLE_ICEBERG_WAREHOUSE,
//! DUCKLE_ICEBERG_S3_ENDPOINT, DUCKLE_ICEBERG_S3_KEY and DUCKLE_ICEBERG_S3_SECRET.
//! Without them the tests skip. The catalog the Iceberg project tests against:
//!
//!   docker network create ice
//!   docker run -d --network ice --name minio -p 59000:9000 \
//!     -e MINIO_ROOT_USER=admin -e MINIO_ROOT_PASSWORD=password minio/minio server /data
//!   (create a bucket named `warehouse`)
//!   docker run -d --network ice -p 58181:8181 -e AWS_ACCESS_KEY_ID=admin \
//!     -e AWS_SECRET_ACCESS_KEY=password -e AWS_REGION=us-east-1 \
//!     -e CATALOG_WAREHOUSE=s3://warehouse/ \
//!     -e CATALOG_IO__IMPL=org.apache.iceberg.aws.s3.S3FileIO \
//!     -e CATALOG_S3_ENDPOINT=http://minio:9000 -e CATALOG_S3_PATH__STYLE__ACCESS=true \
//!     apache/iceberg-rest-fixture
//!
//! Each test writes its own table, so they can run at once.

use duckle_duckdb_engine::{DuckdbEngine, PipelineDoc};
use serde_json::{json, Value};
use std::path::Path;

struct Rest {
    engine: DuckdbEngine,
    uri: String,
    warehouse: String,
    endpoint: String,
    key: String,
    secret: String,
}

fn rest() -> Option<Rest> {
    let bin = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists())?;
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some(Rest {
        engine: DuckdbEngine::new(bin.into()),
        uri: var("DUCKLE_ICEBERG_REST_URI")?,
        warehouse: var("DUCKLE_ICEBERG_WAREHOUSE")?,
        endpoint: var("DUCKLE_ICEBERG_S3_ENDPOINT")?,
        key: var("DUCKLE_ICEBERG_S3_KEY")?,
        secret: var("DUCKLE_ICEBERG_S3_SECRET")?,
    })
}

macro_rules! rest_or_skip {
    () => {
        match rest() {
            Some(r) => r,
            None => {
                eprintln!("skipping: set DUCKLE_ICEBERG_REST_URI and the rest (see the module docs)");
                return;
            }
        }
    };
}

fn node(id: &str, component: &str, props: Value) -> Value {
    json!({ "id": id, "position": { "x": 0, "y": 0 },
            "data": { "label": id, "componentId": component, "properties": props } })
}

impl Rest {
    /// A node's REST-catalog properties, with the storage credentials the
    /// catalog does not vend.
    fn props(&self, table: &str, extra: Value) -> Value {
        let mut p = json!({
            "catalog": "rest", "catalogUri": self.uri, "warehouse": self.warehouse,
            // A namespace per table: two tests creating one new namespace at the
            // same moment race, and the catalog answers the loser with a 409.
            "namespace": format!("duckle_test_{table}"), "table": table,
            "accessKey": self.key, "secretKey": self.secret, "endpoint": self.endpoint,
            "urlStyle": "path", "useSsl": "false", "region": "us-east-1"
        });
        if let (Some(p), Some(e)) = (p.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        p
    }

    fn write(&self, csv: &str, table: &str, extra: Value) -> duckle_duckdb_engine::RunResult {
        let doc: PipelineDoc = serde_json::from_value(json!({
            "nodes": [
                node("s", "src.csv", json!({ "path": csv, "hasHeader": true })),
                node("k", "snk.iceberg", self.props(table, extra)),
            ],
            "edges": [{ "id": "e", "source": "s", "target": "k" }]
        }))
        .unwrap();
        self.engine.execute_pipeline(&doc)
    }

    /// Read the table back through the REST source, into a CSV.
    fn read(&self, table: &str, out: &str) -> duckle_duckdb_engine::RunResult {
        let doc: PipelineDoc = serde_json::from_value(json!({
            "nodes": [
                node("r", "src.iceberg", self.props(table, json!({}))),
                node("k", "snk.csv", json!({ "path": out, "hasHeader": true })),
            ],
            "edges": [{ "id": "e", "source": "r", "target": "k" }]
        }))
        .unwrap();
        self.engine.execute_pipeline(&doc)
    }
}

fn csv(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p.to_string_lossy().replace('\\', "/")
}

fn lines(path: &str) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// A missing table is created in the catalog on the first run and appended to
/// after, matched by column name; the REST source reads it back.
#[test]
fn a_rest_catalog_table_is_created_then_appended() {
    let r = rest_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    let table = "orders_append";
    let first = csv(tmp.path(), "a.csv", "id,v\n1,a\n2,b\n");
    let _ = r.write(&first, table, json!({ "mode": "overwrite" }));
    let again = r.write(&first, table, json!({ "mode": "overwrite" }));
    assert_eq!(again.status, "ok", "{:?}", again.error);
    // Columns in another order land by name.
    let reordered = csv(tmp.path(), "b.csv", "v,id\nc,3\n");
    let appended = r.write(&reordered, table, json!({}));
    assert_eq!(appended.status, "ok", "append: {:?}", appended.error);
    let out = tmp.path().join("back.csv").to_string_lossy().replace('\\', "/");
    let back = r.read(table, &out);
    assert_eq!(back.status, "ok", "read back: {:?}", back.error);
    let mut got = lines(&out);
    got.sort();
    assert_eq!(got, vec!["1,a", "2,b", "3,c"], "overwrite kept one copy, append added by name");
}

/// Overwrite replaces the table rather than adding to it.
#[test]
fn overwrite_replaces_what_was_there() {
    let r = rest_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    let table = "orders_overwrite";
    let big = csv(tmp.path(), "a.csv", "id\n1\n2\n3\n");
    assert_eq!(r.write(&big, table, json!({ "mode": "overwrite" })).status, "ok");
    let small = csv(tmp.path(), "b.csv", "id\n9\n");
    let res = r.write(&small, table, json!({ "mode": "overwrite" }));
    assert_eq!(res.status, "ok", "{:?}", res.error);
    let out = tmp.path().join("back.csv").to_string_lossy().replace('\\', "/");
    assert_eq!(r.read(table, &out).status, "ok");
    assert_eq!(lines(&out), vec!["9"]);
}

/// OAuth2 client credentials reach the catalog through its token endpoint.
#[test]
fn oauth2_client_credentials_are_used() {
    let r = rest_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    let rows = csv(tmp.path(), "a.csv", "id\n1\n");
    let res = r.write(
        &rows,
        "orders_oauth",
        json!({
            "mode": "overwrite", "authType": "oauth2", "clientId": "duckle", "clientSecret": "s3cret",
            "oauth2ServerUri": format!("{}/v1/oauth/tokens", r.uri.trim_end_matches('/'))
        }),
    );
    assert_eq!(res.status, "ok", "{:?}", res.error);
    // Status alone is not proof: the table has to be in the catalog.
    let out = tmp.path().join("back.csv").to_string_lossy().replace('\\', "/");
    assert_eq!(r.read("orders_oauth", &out).status, "ok");
    assert_eq!(lines(&out), vec!["1"]);
    // And the credentials are really used: this catalog also answers requests
    // with no token at all, so only a token endpoint that cannot be reached
    // shows the OAuth2 flow is in the path.
    let refused = r.write(
        &rows,
        "orders_oauth",
        json!({
            "authType": "oauth2", "clientId": "duckle", "clientSecret": "s3cret",
            "oauth2ServerUri": "http://127.0.0.1:1/v1/oauth/tokens"
        }),
    );
    assert_ne!(refused.status, "ok", "a token endpoint that cannot be reached must fail the write");
}

/// Appending to a table that does not exist yet creates it and writes each row
/// once: creating it FROM the rows and then inserting them would double them.
#[test]
fn appending_to_a_missing_table_writes_each_row_once() {
    let r = rest_or_skip!();
    let tmp = tempfile::tempdir().unwrap();
    // A name no earlier run can have used, so the table really is missing.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let table = format!("orders_new_{}_{nanos}", std::process::id());
    let table = table.as_str();
    let rows = csv(tmp.path(), "a.csv", "id\n1\n2\n");
    let res = r.write(&rows, table, json!({ "mode": "append" }));
    assert_eq!(res.status, "ok", "{:?}", res.error);
    let out = tmp.path().join("back.csv").to_string_lossy().replace('\\', "/");
    assert_eq!(r.read(table, &out).status, "ok");
    assert_eq!(lines(&out), vec!["1", "2"]);
}

/// Path mode with no path wrote an Iceberg table into whatever directory the
/// process happened to be in, and reported success.
#[test]
fn a_path_mode_sink_without_a_path_is_refused() {
    let Some(bin) = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists()) else {
        eprintln!("skipping: set DUCKLE_DUCKDB_BIN to run");
        return;
    };
    let engine = DuckdbEngine::new(bin.into());
    let tmp = tempfile::tempdir().unwrap();
    let rows = csv(tmp.path(), "a.csv", "id\n1\n");
    let doc: PipelineDoc = serde_json::from_value(json!({
        "nodes": [
            node("s", "src.csv", json!({ "path": rows, "hasHeader": true })),
            node("k", "snk.iceberg", json!({})),
        ],
        "edges": [{ "id": "e", "source": "s", "target": "k" }]
    }))
    .unwrap();
    let res = engine.execute_pipeline(&doc);
    assert_ne!(res.status, "ok", "an Iceberg sink with nowhere to write must not succeed");
    assert!(res.error.unwrap_or_default().contains("path"));
}
