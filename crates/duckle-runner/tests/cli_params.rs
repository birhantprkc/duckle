//! `duckle-runner --param NAME=VALUE`, run as the real binary (#317).

use std::path::Path;
use std::process::Command;

fn duckdb() -> Option<String> {
    std::env::var("DUCKLE_DUCKDB_BIN").ok().filter(|b| Path::new(b).exists())
}

/// A pipeline whose one declared parameter picks the value it writes.
fn workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("p.json"),
        r#"{"nodes":[
             {"id":"n","position":{"x":0,"y":0},
              "data":{"label":"n","componentId":"src.inline",
                      "properties":{"columns":[{"key":"region","value":"${region}"}]}}},
             {"id":"k","position":{"x":1,"y":0},
              "data":{"label":"k","componentId":"snk.csv",
                      "properties":{"path":"out.csv","hasHeader":true}}}],
           "edges":[{"id":"e","source":"n","target":"k"}],
           "parameters":{"region":{"type":"string","enum":["eu","us"],"required":true}}}"#,
    )
    .unwrap();
    tmp
}

fn run(dir: &Path, extra: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_duckle-runner"))
        .arg("p.json")
        .args(["--workspace", "."])
        .args(extra)
        .current_dir(dir)
        .output()
        .expect("the runner starts");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.code(), text)
}

/// A headless run takes its declared parameters on the command line, through
/// the same typed boundary every other surface uses. Before this the CLI had
/// no way to supply one at all: a pipeline with a required parameter could
/// run anywhere except the command line.
#[test]
fn a_parameter_given_on_the_command_line_reaches_the_run() {
    let Some(_) = duckdb() else {
        eprintln!("skipping: set DUCKLE_DUCKDB_BIN to run");
        return;
    };
    let ws = workspace();
    let (code, out) = run(ws.path(), &["--param", "region=eu"]);
    assert_eq!(code, Some(0), "{out}");
    let written = std::fs::read_to_string(ws.path().join("out.csv")).unwrap_or_default();
    assert!(written.contains("eu"), "the value reached the run: {written}");
}

/// A value the declaration does not allow is refused before anything runs,
/// exactly as it is on the other surfaces.
#[test]
fn a_value_outside_the_declared_set_is_refused() {
    let Some(_) = duckdb() else {
        eprintln!("skipping: set DUCKLE_DUCKDB_BIN to run");
        return;
    };
    let ws = workspace();
    let (code, out) = run(ws.path(), &["--param", "region=mars"]);
    assert_ne!(code, Some(0), "{out}");
    assert!(out.contains("region"), "the refusal names the parameter: {out}");
    assert!(!ws.path().join("out.csv").exists(), "nothing was written");
}

/// A malformed or repeated --param is a usage error, not a guess.
#[test]
fn a_malformed_or_repeated_param_is_a_usage_error() {
    let Some(_) = duckdb() else {
        eprintln!("skipping: set DUCKLE_DUCKDB_BIN to run");
        return;
    };
    let ws = workspace();
    let (code, out) = run(ws.path(), &["--param", "region"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("NAME=VALUE"), "{out}");
    let (code, out) = run(ws.path(), &["--param", "region=eu", "--param", "region=us"]);
    assert_eq!(code, Some(2), "which of two values was meant cannot be guessed: {out}");
    assert!(out.contains("region"), "{out}");
}
