//! SharePoint Server 2019 over its REST API, signed in with NTLM.
//!
//! Runs against tests/sharepoint_mock.py, which answers SharePoint's REST
//! shapes behind a real NTLM acceptor (pyspnego) - one that curl's own
//! `--ntlm` signs in to, so it judges the client rather than agreeing with it.
//! Set DUCKLE_SHAREPOINT_URL to the site (http://127.0.0.1:8765/sites/team);
//! the user is CONTOSO\alice with password P@ssw0rd! unless
//! DUCKLE_SHAREPOINT_USER / DUCKLE_SHAREPOINT_PASSWORD say otherwise.

use duckle_duckdb_engine::{DuckdbEngine, PipelineDoc};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::Path;

struct Site {
    engine: DuckdbEngine,
    bin: String,
    url: String,
    user: String,
    password: String,
}

fn site() -> Option<Site> {
    let bin = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists())?;
    let Some(url) = std::env::var("DUCKLE_SHAREPOINT_URL").ok().filter(|u| !u.is_empty()) else {
        eprintln!("skipping: set DUCKLE_SHAREPOINT_URL to run against tests/sharepoint_mock.py");
        return None;
    };
    Some(Site {
        engine: DuckdbEngine::new(bin.clone().into()),
        bin,
        url,
        user: std::env::var("DUCKLE_SHAREPOINT_USER").unwrap_or_else(|_| r"CONTOSO\alice".into()),
        password: std::env::var("DUCKLE_SHAREPOINT_PASSWORD").unwrap_or_else(|_| "P@ssw0rd!".into()),
    })
}

impl Site {
    fn props(&self, extra: Value) -> Value {
        let mut p = json!({ "siteUrl": self.url, "username": self.user, "password": self.password });
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    }

    /// What the mock holds, read over plain HTTP from its unauthenticated
    /// test endpoint, so the check does not go through the code under test.
    fn state(&self) -> Value {
        let rest = self.url.strip_prefix("http://").expect("the mock is plain http");
        let host = rest.split('/').next().unwrap();
        let mut s = std::net::TcpStream::connect(host).unwrap();
        write!(s, "GET /_test/state HTTP/1.0\r\nHost: {host}\r\n\r\n").unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    fn rows(&self, sql: &str) -> Vec<Value> {
        let out = std::process::Command::new(&self.bin)
            .args([":memory:", "-json", "-c", sql])
            .output()
            .expect("duckdb runs");
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_default()
    }

    /// Run `from` into a Parquet file and return that file's path.
    fn read(&self, dir: &Path, from: Value) -> (duckle_duckdb_engine::RunResult, String) {
        let out = dir.join("out.parquet").to_string_lossy().replace('\\', "/");
        let r = self.engine.execute_pipeline(&doc(
            json!([
                node("s", "src.sharepoint", from),
                node("k", "snk.parquet", json!({ "path": out })),
            ]),
            json!([{ "id": "e", "source": "s", "target": "k" }]),
        ));
        (r, out)
    }

    fn write(&self, sql: &str, to: Value) -> duckle_duckdb_engine::RunResult {
        self.engine.execute_pipeline(&doc(
            json!([
                node("s", "code.sql", json!({ "sql": sql })),
                node("w", "snk.sharepoint", to),
            ]),
            json!([{ "id": "e", "source": "s", "target": "w" }]),
        ))
    }
}

fn doc(nodes: Value, edges: Value) -> PipelineDoc {
    serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).unwrap()
}

fn node(id: &str, component: &str, props: Value) -> Value {
    json!({ "id": id, "position": { "x": 0, "y": 0 },
            "data": { "label": id, "componentId": component, "properties": props } })
}

/// Every page of a list, followed through odata.nextLink, each row once.
#[test]
fn a_list_reads_every_page_signed_in_with_ntlm() {
    let Some(s) = site() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (r, out) = s.read(tmp.path(), s.props(json!({ "listName": "Orders", "pageSize": 2 })));
    assert_eq!(r.status, "ok", "{:?}", r.error);
    let rows = s.rows(&format!("SELECT ID, Title, Amount, Region FROM '{out}' ORDER BY ID"));
    assert_eq!(rows.len(), 5, "three pages of two, two, one: {rows:?}");
    assert_eq!(rows[0]["Title"], "Zoë's order", "an accent and an apostrophe, as stored");
    assert_eq!(rows[3]["Amount"], 100.0);

    // Only the columns asked for, and the filter handed to SharePoint as written.
    let (r, out) = s.read(
        tmp.path(),
        s.props(json!({ "listName": "Orders", "select": "ID,Title", "filter": "Region eq 'North'" })),
    );
    assert_eq!(r.status, "ok", "{:?}", r.error);
    let cols: Vec<String> = s
        .rows(&format!("SELECT column_name FROM (DESCRIBE SELECT * FROM '{out}')"))
        .iter()
        .map(|c| c["column_name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(cols, vec!["ID", "Title"]);
    let sent = s.state()["requests"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|q| q["path"].as_str().unwrap_or("").ends_with("/items"))
        .cloned()
        .unwrap();
    assert_eq!(sent["query"]["$filter"], json!(["Region eq 'North'"]), "{sent}");
}

/// A wrong password is refused, and the run says it was the sign-in.
#[test]
fn a_wrong_password_is_refused_as_a_sign_in_failure() {
    let Some(s) = site() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let mut props = s.props(json!({ "listName": "Orders" }));
    props["password"] = json!("not-the-password");
    let (r, _) = s.read(tmp.path(), props);
    assert_ne!(r.status, "ok");
    let e = format!("{:?}", r.error);
    assert!(e.contains("401") && e.to_lowercase().contains("sign"), "{e}");
    assert!(!e.contains("not-the-password"), "the password is not in the error: {e}");
}

/// SharePoint's own message for a list that is not there reaches the run.
#[test]
fn a_missing_list_is_named() {
    let Some(s) = site() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (r, _) = s.read(tmp.path(), s.props(json!({ "listName": "Nope" })));
    assert_ne!(r.status, "ok");
    assert!(format!("{:?}", r.error).contains("List 'Nope' does not exist"), "{:?}", r.error);
}

/// Rows become list items, each field as its own JSON type: a number as a
/// number, not a string SharePoint would parse in the site's locale, and a
/// date/time in ISO 8601.
#[test]
fn rows_written_become_list_items() {
    let Some(s) = site() else { return };
    let r = s.write(
        "SELECT * FROM (VALUES ('Zoë''s import', 4.5, true, TIMESTAMP '2026-09-24 10:11:12'), \
                               ('Plain', 2.0, false, NULL)) t(Title, Amount, Done, Due)",
        s.props(json!({ "listName": "Imported" })),
    );
    assert_eq!(r.status, "ok", "{:?}", r.error);
    let items = s.state()["lists"]["Imported"].clone();
    let got: Vec<(Value, Value, Value, Value)> = items
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["Title"] == "Zoë's import" || i["Title"] == "Plain")
        .map(|i| (i["Title"].clone(), i["Amount"].clone(), i["Done"].clone(), i["Due"].clone()))
        .collect();
    assert!(
        got.contains(&(json!("Zoë's import"), json!(4.5), json!(true), json!("2026-09-24T10:11:12"))),
        "{items}"
    );
    assert!(got.contains(&(json!("Plain"), json!(2.0), json!(false), Value::Null)), "{items}");
}

/// A file in a document library reads as rows, by its extension.
#[test]
fn a_library_file_reads_as_rows() {
    let Some(s) = site() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (r, out) = s.read(
        tmp.path(),
        s.props(json!({ "mode": "file", "fileUrl": "/sites/team/Shared Documents/orders.csv" })),
    );
    assert_eq!(r.status, "ok", "{:?}", r.error);
    let rows = s.rows(&format!("SELECT name, amount FROM '{out}' ORDER BY id"));
    assert_eq!(rows, vec![json!({ "name": "Zoë", "amount": 12.5 }), json!({ "name": "Ana", "amount": 7.0 })]);
}

/// The output uploads as a library file; with overwrite off, an existing
/// file is left alone and the run says so.
#[test]
fn the_output_uploads_as_a_library_file() {
    let Some(s) = site() else { return };
    let to = |overwrite: bool| {
        s.props(json!({ "mode": "file", "folderUrl": "/sites/team/Shared Documents",
                        "fileName": "upload.csv", "overwrite": overwrite }))
    };
    let r = s.write("SELECT 1 AS id, 'Zoë' AS name", to(true));
    assert_eq!(r.status, "ok", "{:?}", r.error);
    let stored = s.state()["files"]["/sites/team/Shared Documents/upload.csv"].clone();
    assert_eq!(stored.as_str().unwrap().replace("\r\n", "\n"), "id,name\n1,Zoë\n");

    let again = s.write("SELECT 2 AS id, 'x' AS name", to(false));
    assert_ne!(again.status, "ok", "an existing file was replaced with overwrite off");
    assert!(format!("{:?}", again.error).contains("already exists"), "{:?}", again.error);
    let kept = s.state()["files"]["/sites/team/Shared Documents/upload.csv"].clone();
    assert_eq!(kept, stored);
}
