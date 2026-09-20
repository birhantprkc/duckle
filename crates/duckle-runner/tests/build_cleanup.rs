//! `duckle-runner build`, run as the real binary.

/// A build that fails left its staging tree in the temp directory for good.
///
/// The tree is removed on exactly one line, after the artifact has been
/// written, so every one of the twenty-two `?`s between it and the mkdir - a
/// missing stub, an unreadable duckdb, a leak guard, a bad context - kept the
/// staged duckdb binary, pipeline and contexts alive. That is tens of megabytes
/// per failed attempt, and a build is something an operator retries.
#[test]
fn a_failed_build_leaves_no_staging_tree_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("pipelines")).unwrap();
    std::fs::write(
        ws.join("repository.json"),
        r#"[{"id":"probe","name":"staging leak probe","type":"pipeline"}]"#,
    )
    .unwrap();
    std::fs::write(
        ws.join("pipelines").join("probe.json"),
        r#"{"nodes":[],"edges":[]}"#,
    )
    .unwrap();

    // The child gets a temp directory of its own, so whatever is left in it
    // belongs to this build and to no other test or run on the machine.
    let temp = tmp.path().join("temp");
    std::fs::create_dir_all(&temp).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_duckle-runner"))
        .arg("build")
        .arg("--workspace")
        .arg(&ws)
        .args(["--pipeline-id", "probe"])
        .arg("--out")
        .arg(tmp.path().join("probe.out"))
        // Fails in `resolve_stub`, which sits after the staging tree is built
        // and before the line that used to be its only remover.
        .arg("--stub")
        .arg(tmp.path().join("no-such-stub"))
        .env("TMP", &temp)
        .env("TEMP", &temp)
        .env("TMPDIR", &temp)
        .output()
        .expect("the runner starts");

    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "the build was meant to fail: {stderr}");
    assert!(
        stderr.contains("read stub"),
        "the build has to fail AFTER staging for this test to mean anything: {stderr}"
    );

    let left: Vec<String> = std::fs::read_dir(&temp)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("duckle-build-"))
        .collect();
    assert!(
        left.is_empty(),
        "the failed build left its staging tree behind: {left:?}"
    );
}
