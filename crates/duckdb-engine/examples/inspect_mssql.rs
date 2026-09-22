// Repro harness for issue #141: autodetect ("inspect") on a SQL Server source
// panics even on a trivial 3-int table. Calls Engine::inspect exactly like the
// desktop autodetect_schema command, so it exercises inspect_driver_source +
// its catch_unwind. The default panic hook prints the real panic payload to
// stderr BEFORE catch_unwind swallows it (the GUI just has no console).
//
//   DUCKLE_DUCKDB_BIN=.duckdb-cli-v1.5.5/duckdb.exe \
//   cargo run --example inspect_mssql -p duckle-duckdb-engine
use duckle_duckdb_engine::DuckdbEngine;
use serde_json::json;

fn main() {
    let duckdb = std::env::var("DUCKLE_DUCKDB_BIN")
        .unwrap_or_else(|_| r".duckdb-cli-v1.5.5\duckdb.exe".into());
    let table = std::env::var("MSSQL_TABLE").unwrap_or_else(|_| "three_ints".into());
    let eng = DuckdbEngine::new(duckdb.into());

    // Exactly what the FE autodetect sends for a src.sqlserver node: connection
    // props + tableName (or a raw query via MSSQL_QUERY), so inspect resolves
    // the schema through sys.dm_exec_describe_first_result_set.
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let port: u64 = env("MSSQL_PORT", "1433").parse().unwrap_or(1433);
    let mut opts = json!({
        "host": env("MSSQL_HOST", "localhost"),
        "port": port,
        "database": env("MSSQL_DB", "duckletest"),
        "user": env("MSSQL_USER", "sa"),
        "password": env("MSSQL_PASSWORD", ""),
        "encrypt": false, "trustCert": true,
        "schema": "dbo", "tableName": table,
    });
    if let Ok(q) = std::env::var("MSSQL_QUERY") {
        opts.as_object_mut().unwrap().insert("query".into(), json!(q));
    }

    eprintln!("--- inspect(\"sqlserver\", {}) ---", std::env::var("MSSQL_QUERY").map(|q| format!("query={}", q)).unwrap_or_else(|_| format!("tableName={}", table)));
    match eng.inspect("sqlserver", opts) {
        Ok(insp) => {
            eprintln!("OK: {} columns, {} sample rows", insp.schema.len(), insp.sample_rows.len());
            for c in &insp.schema {
                eprintln!("  {:<20} {}", c.name, c.data_type.name());
            }
            if let Some(first) = insp.sample_rows.first() {
                eprintln!("first sample row: {}", first);
            }
        }
        Err(e) => eprintln!("ERR: {}", e),
    }
}
