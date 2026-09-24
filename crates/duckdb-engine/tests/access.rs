//! Microsoft Access (.accdb / .mdb) through the Access ODBC driver, end to end.
//!
//! Windows only, and only where the 64-bit Access Database Engine is
//! installed: it provides both the ODBC driver these nodes use and the OLE DB
//! provider the checks below use. The fixtures are made and read back through
//! ADODB, Microsoft's own client, so what is asserted is what Access itself
//! holds, not what this crate thinks it wrote.
#![cfg(all(windows, feature = "odbc"))]

use duckle_duckdb_engine::{DuckdbEngine, PipelineDoc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn engine() -> Option<(DuckdbEngine, String)> {
    let bin = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists())?;
    Some((DuckdbEngine::new(bin.clone().into()), bin))
}

/// Run PowerShell and return stdout, or None when the script did not finish.
///
/// Finishing is judged by a marker printed last, not by the exit code:
/// Windows PowerShell 5.1 does every step against the Access engine and then
/// dies in COM teardown with 0xC0000005 at exit. PowerShell 7 exits cleanly,
/// so it is tried first.
fn ps(script: &str) -> Option<String> {
    const DONE: &str = "__duckle_done__";
    let script = format!("{script}; [Console]::WriteLine('{DONE}')");
    for shell in ["pwsh", "powershell"] {
        let Ok(out) = std::process::Command::new(shell)
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
        else {
            continue;
        };
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        return match stdout.rfind(DONE) {
            Some(end) => Some(stdout[..end].trim().to_string()),
            None => {
                eprintln!("{shell}: {}{}", stdout, String::from_utf8_lossy(&out.stderr));
                None
            }
        };
    }
    None
}

fn ace(path: &Path, password: Option<&str>) -> String {
    let mut s = format!("Provider=Microsoft.ACE.OLEDB.12.0;Data Source={}", path.display());
    if let Some(p) = password {
        s.push_str(&format!(";Jet OLEDB:Database Password={p}"));
    }
    s
}

/// A new database file holding `statements`, made by Access itself. None when
/// the Access Database Engine is not installed, which is a skip, not a failure.
fn access_file(dir: &Path, name: &str, password: Option<&str>, statements: &[&str]) -> Option<PathBuf> {
    let path = dir.join(name);
    let conn = ace(&path, password).replace('\'', "''");
    let mut script = format!(
        "$ErrorActionPreference='Stop'; $c = New-Object -ComObject ADOX.Catalog; $null = $c.Create('{conn}'); $c.ActiveConnection.Close();\
         $a = New-Object -ComObject ADODB.Connection; $a.Open('{conn}');"
    );
    for s in statements {
        script.push_str(&format!(" $null = $a.Execute('{}');", s.replace('\'', "''")));
    }
    script.push_str(" $a.Close();");
    ps(&script)?;
    Some(path)
}

/// Rows of `sql` as Access itself answers it, one line per row, fields joined
/// by `|`, dates in ISO form.
fn access_rows(path: &Path, password: Option<&str>, sql: &str) -> Vec<String> {
    let conn = ace(path, password).replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference='Stop'; [Console]::OutputEncoding = [Text.Encoding]::UTF8;\
         $a = New-Object -ComObject ADODB.Connection; $a.Open('{conn}');\
         $r = $a.Execute('{sql}');\
         while (-not $r.EOF) {{ $f = @(); foreach ($x in $r.Fields) {{ $v = $x.Value;\
           if ($v -is [datetime]) {{ $v = $v.ToString('yyyy-MM-dd HH:mm:ss') }} $f += [string]$v }};\
           [Console]::WriteLine(($f -join '|')); $null = $r.MoveNext() }}; $a.Close();",
        sql = sql.replace('\'', "''")
    );
    ps(&script).unwrap_or_default().lines().map(str::to_string).collect()
}

fn doc(nodes: Value, edges: Value) -> PipelineDoc {
    serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).unwrap()
}

fn node(id: &str, component: &str, props: Value) -> Value {
    json!({ "id": id, "position": { "x": 0, "y": 0 },
            "data": { "label": id, "componentId": component, "properties": props } })
}

fn edge(from: &str, to: &str) -> Value {
    json!({ "id": format!("e_{from}_{to}"), "source": from, "target": to })
}

fn query(bin: &str, sql: &str) -> Vec<Value> {
    let out = std::process::Command::new(bin)
        .args([":memory:", "-json", "-c", sql])
        .output()
        .expect("duckdb runs");
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_default()
}

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

const PEOPLE: &[&str] = &[
    "CREATE TABLE People (ID COUNTER PRIMARY KEY, Name TEXT(50), Code TEXT(10), Qty LONG, \
     Price DOUBLE, Amount CURRENCY, Active YESNO, Joined DATETIME, Notes MEMO)",
    "INSERT INTO People (Name, Code, Qty, Price, Amount, Active, Joined, Notes) \
     VALUES ('Zoë Müller', '007', 3, 2.5, 12.34, True, #2026-09-24 10:11:12#, 'short')",
    "INSERT INTO People (Name, Code, Qty, Price, Amount, Active, Joined, Notes) \
     VALUES ('Ana', '010', 1, 0.125, 1000.5, False, #1999-12-31 00:00:00#, NULL)",
];

/// A table read keeps what Access holds: its column types, text with accents
/// in it, and a code whose leading zeros are part of the value.
#[test]
fn a_table_reads_with_its_types_and_its_text_intact() {
    let Some((engine, bin)) = engine() else {
        eprintln!("skipping: set DUCKLE_DUCKDB_BIN to run");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let Some(db) = access_file(tmp.path(), "people.accdb", None, PEOPLE) else {
        eprintln!("skipping: the Access Database Engine is not installed");
        return;
    };
    let out = fwd(&tmp.path().join("out.parquet"));
    let d = doc(
        json!([
            node("a", "src.access", json!({ "path": fwd(&db), "tableName": "People" })),
            node("k", "snk.parquet", json!({ "path": out })),
        ]),
        json!([edge("a", "k")]),
    );
    let r = engine.execute_pipeline(&d);
    assert_eq!(r.status, "ok", "{:?}", r.error);

    let rows = query(&bin, &format!("SELECT * FROM '{out}' ORDER BY ID"));
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["Name"], "Zoë Müller", "text with accents, as Access holds it");
    assert_eq!(rows[0]["Code"], "007", "a code keeps its leading zeros");
    assert_eq!(rows[0]["Active"], true);
    assert_eq!(rows[1]["Active"], false);
    assert_eq!(rows[0]["Joined"], "2026-09-24 10:11:12");
    assert!(rows[1]["Notes"].is_null());

    let types: std::collections::HashMap<String, String> =
        query(&bin, &format!("SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM '{out}')"))
            .into_iter()
            .map(|r| (r["column_name"].as_str().unwrap().into(), r["column_type"].as_str().unwrap().into()))
            .collect();
    assert_eq!(types["Qty"], "INTEGER", "{types:?}");
    assert_eq!(types["Price"], "DOUBLE", "{types:?}");
    assert!(types["Amount"].starts_with("DECIMAL"), "currency is exact: {types:?}");
    assert_eq!(types["Active"], "BOOLEAN", "{types:?}");
    assert_eq!(types["Joined"], "TIMESTAMP", "{types:?}");
    assert_eq!(types["Code"], "VARCHAR", "{types:?}");
}

/// A query runs in Access's own SQL, brackets and all.
#[test]
fn a_query_runs_in_access_sql() {
    let Some((engine, bin)) = engine() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let Some(db) = access_file(tmp.path(), "people.accdb", None, PEOPLE) else { return };
    let out = fwd(&tmp.path().join("out.parquet"));
    let d = doc(
        json!([
            node("a", "src.access", json!({ "path": fwd(&db),
                "query": "SELECT [Name] FROM [People] WHERE [Qty] > 2" })),
            node("k", "snk.parquet", json!({ "path": out })),
        ]),
        json!([edge("a", "k")]),
    );
    let r = engine.execute_pipeline(&d);
    assert_eq!(r.status, "ok", "{:?}", r.error);
    let rows = query(&bin, &format!("SELECT * FROM '{out}'"));
    assert_eq!(rows, vec![json!({ "Name": "Zoë Müller" })]);
}

/// A database with a password opens with it, and says so without it.
#[test]
fn a_password_protected_database_needs_its_password() {
    let Some((engine, _)) = engine() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let Some(db) = access_file(tmp.path(), "locked.accdb", Some("s3cret"), PEOPLE) else { return };
    let run = |password: Option<&str>| {
        let mut props = json!({ "path": fwd(&db), "tableName": "People" });
        if let Some(p) = password {
            props["password"] = json!(p);
        }
        let out = fwd(&tmp.path().join(format!("out_{}.parquet", password.is_some())));
        engine.execute_pipeline(&doc(
            json!([node("a", "src.access", props), node("k", "snk.parquet", json!({ "path": out }))]),
            json!([edge("a", "k")]),
        ))
    };
    let without = run(None);
    assert_ne!(without.status, "ok", "opened a locked database with no password");
    let with = run(Some("s3cret"));
    assert_eq!(with.status, "ok", "{:?}", with.error);
}

/// Rows written land in Access as Access types, read back by Access itself:
/// append adds, overwrite replaces, and a table that is not there is created.
#[test]
fn rows_written_are_what_access_reads_back() {
    let Some((engine, _)) = engine() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let Some(db) = access_file(tmp.path(), "target.accdb", None, &[]) else { return };
    let write = |mode: &str| {
        engine.execute_pipeline(&doc(
            json!([
                node("s", "code.sql", json!({ "sql":
                    "SELECT * FROM (VALUES (1, 'Zoë', '007', 2.5, 12.34::DECIMAL(10,2), true, TIMESTAMP '2026-09-24 10:11:12', DATE '2026-01-02'), \
                                           (2, 'O''Brien', '010', NULL, 0.10::DECIMAL(10,2), false, NULL, NULL)) \
                     t(id, name, code, price, amount, active, joined, born)" })),
                node("w", "snk.access", json!({ "path": fwd(&db), "tableName": "Out", "mode": mode })),
            ]),
            json!([edge("s", "w")]),
        ))
    };
    let r = write("append");
    assert_eq!(r.status, "ok", "{:?}", r.error);
    let sql = "SELECT id, name, code, price, amount, active, joined, born FROM [Out] ORDER BY id";
    assert_eq!(
        access_rows(&db, None, sql),
        vec![
            "1|Zoë|007|2.5|12.34|True|2026-09-24 10:11:12|2026-01-02 00:00:00".to_string(),
            // The decimal keeps its declared scale: DECIMAL(10,2) in, 0.10 out.
            "2|O'Brien|010||0.10|False||".to_string(),
        ]
    );

    let again = write("append");
    assert_eq!(again.status, "ok", "{:?}", again.error);
    assert_eq!(access_rows(&db, None, "SELECT count(*) FROM [Out]"), vec!["4".to_string()]);

    let replaced = write("overwrite");
    assert_eq!(replaced.status, "ok", "{:?}", replaced.error);
    assert_eq!(access_rows(&db, None, "SELECT count(*) FROM [Out]"), vec!["2".to_string()]);

    // An upstream that produced nothing leaves the table alone rather than
    // emptying it: a source that hiccuped is not a request to delete.
    let empty = engine.execute_pipeline(&doc(
        json!([
            node("s", "code.sql", json!({ "sql": "SELECT 1 AS id WHERE false" })),
            node("w", "snk.access", json!({ "path": fwd(&db), "tableName": "Out", "mode": "overwrite" })),
        ]),
        json!([edge("s", "w")]),
    ));
    assert_eq!(empty.status, "ok", "{:?}", empty.error);
    assert_eq!(access_rows(&db, None, "SELECT count(*) FROM [Out]"), vec!["2".to_string()]);
}

/// A sink pointed at a database file that does not exist yet makes it.
#[test]
fn a_database_file_that_is_not_there_is_created() {
    let Some((engine, _)) = engine() else { return };
    let tmp = tempfile::tempdir().unwrap();
    // The engine is present, which is what the probe checks for.
    if access_file(tmp.path(), "probe.accdb", None, &[]).is_none() {
        return;
    }
    let db = tmp.path().join("new.accdb");
    let r = engine.execute_pipeline(&doc(
        json!([
            node("s", "code.sql", json!({ "sql": "SELECT 1 AS id, 'x' AS v" })),
            node("w", "snk.access", json!({ "path": fwd(&db), "tableName": "T" })),
        ]),
        json!([edge("s", "w")]),
    ));
    assert_eq!(r.status, "ok", "{:?}", r.error);
    assert_eq!(access_rows(&db, None, "SELECT id, v FROM [T]"), vec!["1|x".to_string()]);
}
