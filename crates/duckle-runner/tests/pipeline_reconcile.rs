//! `duckle-runner --pipeline`, run as the real binary.

/// The documented headless deployment is cron calling `--pipeline`, and it was
/// the one surface that never reconciled: `retry`, `serve`, `web`, the desktop
/// and the backfill paths all do. So on a box where CI cancels jobs or the
/// machine reboots, every killed run left a receipt claiming to be in flight and
/// no later run of any pipeline cleared it.
///
/// It compounds rather than just looking untidy: `prune` and `retention` both
/// exclude running receipts from their candidates, so the MAX_RECEIPTS cap never
/// applied to them, the directory grew without bound, and `prune` re-scans all
/// of it on every receipt write. This is also the surface with no console to fix
/// it from.
#[test]
fn a_plain_pipeline_run_reclaims_what_an_earlier_kill_left_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();

    // A receipt exactly as a killed run leaves it: still `running`, owned by a
    // pid that is never alive on either platform.
    let mut abandoned = duckle_duckdb_engine::retry::begin(
        ws,
        "run-abandoned",
        "scheduled",
        "orders",
        &ws.join("pipelines").join("orders.json").display().to_string(),
        "hash",
        None,
    );
    abandoned.pid = Some(u32::MAX);
    duckle_duckdb_engine::retry::write(ws, &abandoned).unwrap();

    // The smallest pipeline that runs: no source, no sink, nothing to execute.
    // What is under test is the reconcile at the start of the path, not the run.
    let pipeline = tmp.path().join("tiny.json");
    std::fs::write(&pipeline, r#"{"nodes":[],"edges":[]}"#).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_duckle-runner"))
        .arg("--pipeline")
        .arg(&pipeline)
        .arg("--workspace")
        .arg(ws)
        .arg("--name")
        .arg("tiny")
        .output()
        .expect("the runner starts");

    let state = duckle_duckdb_engine::retry::load(ws, "run-abandoned")
        .expect("the abandoned receipt is still there")
        .state;
    assert_eq!(
        state,
        duckle_duckdb_engine::retry::INTERRUPTED,
        "a plain --pipeline run left an earlier kill marked in flight, where nothing else \
         in a cron-only workspace will ever clear it; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
