//! "From SQL", end to end: the pipeline built from a query computes what the
//! query computes.

use duckle_duckdb_engine::{DuckdbEngine, PipelineDoc};
use serde_json::{json, Value};
use std::path::Path;

fn engine() -> Option<(DuckdbEngine, String)> {
    let bin = std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists())?;
    Some((DuckdbEngine::new(bin.clone().into()), bin))
}

fn rows_of(bin: &str, sql: &str) -> Vec<Value> {
    let out = std::process::Command::new(bin)
        .args([":memory:", "-json", "-c", sql])
        .output()
        .expect("duckdb runs");
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_default()
}

const QUERY: &str = "WITH paid AS (SELECT * FROM orders WHERE status = 'paid'), \
     totals AS (SELECT customer_id, sum(amount) AS total FROM paid GROUP BY customer_id) \
     SELECT c.name, t.total FROM totals t JOIN customers c ON c.id = t.customer_id ORDER BY c.name";

/// The query and the pipeline built from it, over the same two tables, give
/// the same rows. The generated sources are placeholders; pointing them at
/// data is the user's step, and the test takes it the same way.
#[test]
fn the_pipeline_computes_what_the_query_computes() {
    let Some((engine, bin)) = engine() else {
        eprintln!("skipping: set DUCKLE_DUCKDB_BIN to run");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let file = |name: &str, body: &str| {
        let p = tmp.path().join(name);
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().replace('\\', "/")
    };
    let orders = file(
        "orders.csv",
        "id,customer_id,status,amount\n1,1,paid,10\n2,1,paid,5\n3,2,open,99\n4,2,paid,7\n",
    );
    let customers = file("customers.csv", "id,name\n1,Ada\n2,Bo\n");
    let expected = rows_of(
        &bin,
        &format!(
            "CREATE VIEW orders AS SELECT * FROM read_csv('{orders}'); \
             CREATE VIEW customers AS SELECT * FROM read_csv('{customers}'); {QUERY}"
        ),
    );
    assert_eq!(expected.len(), 2, "the reference result itself: {expected:?}");

    let (mut doc, kept_whole) = engine.pipeline_from_sql(QUERY).expect("the query converts");
    assert!(kept_whole.is_none());
    let steps: Vec<String> = doc["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["data"]["componentId"] == "code.sql")
        .map(|n| n["data"]["label"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(steps, vec!["paid", "totals", "Result"], "one step per CTE, then the SELECT");

    // Point the placeholders at the files, and write the result out.
    for n in doc["nodes"].as_array_mut().unwrap() {
        if n["data"]["componentId"] == "src.duckdb" {
            let table = n["data"]["alias"].as_str().unwrap().to_string();
            n["data"]["componentId"] = json!("src.csv");
            n["data"]["properties"] =
                json!({ "path": if table == "orders" { &orders } else { &customers }, "hasHeader": true });
        }
    }
    let out = tmp.path().join("out.csv").to_string_lossy().replace('\\', "/");
    doc["nodes"].as_array_mut().unwrap().push(json!({
        "id": "k", "position": { "x": 0, "y": 0 },
        "data": { "label": "k", "componentId": "snk.csv", "properties": { "path": out, "hasHeader": true } }
    }));
    doc["edges"].as_array_mut().unwrap().push(json!({ "id": "ek", "source": "result", "target": "k" }));
    let pipeline: PipelineDoc = serde_json::from_value(doc).unwrap();
    let r = engine.execute_pipeline(&pipeline);
    assert_eq!(r.status, "ok", "the generated pipeline runs: {:?}", r.error);
    let got = rows_of(&bin, &format!("SELECT * FROM read_csv('{out}') ORDER BY name"));
    // Compared as text: sum() gives a HUGEINT, which JSON renders as a string,
    // while the pipeline's result came back through a CSV as a number. The
    // values are what is being compared, not how one output format types them.
    let text = |rows: &[Value]| -> Vec<Vec<(String, String)>> {
        rows.iter()
            .map(|r| {
                r.as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
                    .collect()
            })
            .collect()
    };
    assert_eq!(text(&got), text(&expected), "the pipeline's rows are the query's rows");
}

/// A query that will not parse says so, rather than producing an empty graph.
#[test]
fn a_query_that_does_not_parse_is_refused() {
    let Some((engine, _)) = engine() else {
        eprintln!("skipping: set DUCKLE_DUCKDB_BIN to run");
        return;
    };
    let err = engine.pipeline_from_sql("SELEC * FORM x").expect_err("not SQL");
    assert!(err.to_string().to_lowercase().contains("could not be read"), "{err}");
}
