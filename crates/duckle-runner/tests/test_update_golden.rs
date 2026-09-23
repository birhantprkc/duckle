//! `duckle-runner test --update-golden`, run as the real binary.

use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

fn duckdb() -> Option<String> {
    std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists())
}

fn runner(args: &[&str], dir: &Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_duckle-runner"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the runner starts");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// A stale golden fails; `--update-golden` rewrites exactly the rows a run
/// produced and nothing else; the suite then passes.
///
/// #250 asked for this and for it to be explicit, so a plain `test` never
/// writes. What it may touch is narrow on purpose: a case that asserts only
/// structure has no rows to record and is left as written, a case whose run
/// fails is not recorded - a golden is what a WORKING run produced - and every
/// other key in the file keeps its place.
#[test]
fn update_golden_records_what_the_run_produced_and_only_that() {
    let Some(_) = duckdb() else {
        eprintln!("skipping: set DUCKLE_DUCKDB_BIN to run");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("rows.csv"), "id,amt\n1,5\n2,\n3,7\n").unwrap();
    std::fs::write(
        dir.join("orders.json"),
        json!({
            "nodes": [
                { "id": "s", "position": { "x": 0, "y": 0 },
                  "data": { "label": "S", "componentId": "src.csv",
                            "properties": { "path": "rows.csv", "hasHeader": true } } },
                { "id": "f", "position": { "x": 1, "y": 0 },
                  "data": { "label": "F", "componentId": "xf.filter",
                            "properties": { "predicate": "amt IS NOT NULL" } } }
            ],
            "edges": [{ "id": "e", "source": "s", "target": "f" }]
        })
        .to_string(),
    )
    .unwrap();
    let suite = dir.join("orders.test.json");
    std::fs::write(
        &suite,
        serde_json::to_string_pretty(&json!({
            "pipeline": "orders.json",
            "cases": [
                { "name": "keeps rows with an amount",
                  "expect": { "node": "f", "orderBy": ["id"], "rows": [{ "id": 1, "amt": 5 }] } },
                { "name": "counts only",
                  "expect": { "node": "f", "rowCount": 2 } },
                { "name": "asserts on a node that is not there",
                  "expect": { "node": "nope", "rows": [{ "id": 0 }] } }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let before = std::fs::read_to_string(&suite).unwrap();

    // A plain run reports the stale golden and writes nothing.
    let (ok, out) = runner(&["test", "orders.test.json"], dir);
    assert!(!ok, "the stale golden should fail:\n{out}");
    assert_eq!(std::fs::read_to_string(&suite).unwrap(), before, "a plain `test` never writes");

    let (ok, out) = runner(&["test", "--update-golden", "orders.test.json"], dir);
    assert!(!ok, "a case that cannot run is still a failure, update or not:\n{out}");
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&suite).unwrap()).unwrap();
    let cases = after["cases"].as_array().unwrap();
    assert_eq!(
        cases[0]["expect"]["rows"],
        json!([{ "id": 1, "amt": 5 }, { "id": 3, "amt": 7 }]),
        "the golden is what the node produced, typed:\n{out}"
    );
    assert_eq!(
        cases[0]["expect"]["orderBy"],
        json!(["id"]),
        "the case's other settings are untouched"
    );
    assert!(cases[1]["expect"].get("rows").is_none(), "a structure-only case gains no rows");
    assert_eq!(cases[2]["expect"]["rows"], json!([{ "id": 0 }]), "a failed run records nothing");
    let keys: Vec<&str> = after.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["pipeline", "cases"], "the file keeps its shape");

    // Recorded, the good cases pass; the broken one still fails.
    let (_, out) = runner(&["test", "orders.test.json"], dir);
    assert!(out.contains("ok    keeps rows with an amount"), "{out}");
    assert!(out.contains("FAIL  asserts on a node that is not there"), "{out}");
}

/// Recording goldens is a local act, so it will not also print a CI report.
#[test]
fn update_golden_refuses_a_report_format() {
    let Some(_) = duckdb() else {
        eprintln!("skipping: set DUCKLE_DUCKDB_BIN to run");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (ok, out) = runner(&["test", "--update-golden", "--format", "junit", "x.test.json"], tmp.path());
    assert!(!ok);
    assert!(out.contains("prints no report"), "the refusal, not an unknown-flag error: {out}");
}
