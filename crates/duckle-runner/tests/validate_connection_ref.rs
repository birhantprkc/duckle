//! `duckle-runner validate`, run as the real binary, against a saved connection.

/// A node that carries only `connectionRef` validates.
///
/// #166 lets a node hold a reference instead of its auth props, and the saved
/// connection supplies the rest. Every run path resolves those refs BEFORE it
/// compiles; validate did not, so the builders saw a node with no credentials
/// at all and failed it for a field the connection provides:
///
///   config: snk.salesforce: instanceUrl required (e.g. https://acme.my.salesforce.com)
///
/// The pipeline ran perfectly. Measured on the shipped suite under
/// docs/salesforce-sink/live-suite, 11 of its 13 real pipelines failed this way
/// while the two meant to fail - a wrong-kind and a missing ref - were the only
/// honest failures. validate is the gate the CI templates run, so it was
/// rejecting correct work.
#[test]
fn a_node_holding_only_a_connection_ref_validates() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();

    // The connection carries the auth, exactly as the shipped suite's does:
    // client-credentials, where the token response supplies the instance url,
    // so `instanceUrl` on the node is neither present nor needed.
    std::fs::create_dir_all(ws.join("connections")).unwrap();
    std::fs::write(
        ws.join("connections").join("sf.json"),
        r#"{"kind":"salesforce","authMode":"clientCredentials",
            "loginUrl":"https://example.my.salesforce.com",
            "clientId":"cid","clientSecret":"secret"}"#,
    )
    .unwrap();
    std::fs::write(ws.join("rows.csv"), "Name\nAcme\n").unwrap();

    let pipeline = ws.join("insert.json");
    std::fs::write(
        &pipeline,
        r#"{"nodes":[
             {"id":"s","type":"source","position":{"x":0,"y":0},
              "data":{"label":"In","componentId":"src.csv",
                      "properties":{"path":"rows.csv","hasHeader":true}}},
             {"id":"k","type":"sink","position":{"x":200,"y":0},
              "data":{"label":"Out","componentId":"snk.salesforce",
                      "properties":{"connectionRef":"sf","object":"Account",
                                    "operation":"insert","apiVersion":"v60.0"}}}],
           "edges":[{"id":"e1","source":"s","target":"k"}]}"#,
    )
    .unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_duckle-runner"))
        .arg("validate")
        .arg(&pipeline)
        .output()
        .expect("the runner starts");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "a pipeline whose credentials come from a saved connection must validate; \
         stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("instanceUrl required"),
        "validate asked for a field the connection supplies: {stdout}"
    );
}

/// A reference to a connection that is not there still fails.
///
/// The resolution is best effort so that a workspace without the file keeps
/// validating everything else, but it must not turn into "anything with a
/// connectionRef passes": the builder's own error is what should stand.
#[test]
fn a_connection_ref_that_resolves_to_nothing_still_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    std::fs::write(ws.join("rows.csv"), "Name\nAcme\n").unwrap();

    let pipeline = ws.join("insert.json");
    std::fs::write(
        &pipeline,
        r#"{"nodes":[
             {"id":"s","type":"source","position":{"x":0,"y":0},
              "data":{"label":"In","componentId":"src.csv",
                      "properties":{"path":"rows.csv","hasHeader":true}}},
             {"id":"k","type":"sink","position":{"x":200,"y":0},
              "data":{"label":"Out","componentId":"snk.salesforce",
                      "properties":{"connectionRef":"absent","object":"Account",
                                    "operation":"insert","apiVersion":"v60.0"}}}],
           "edges":[{"id":"e1","source":"s","target":"k"}]}"#,
    )
    .unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_duckle-runner"))
        .arg("validate")
        .arg(&pipeline)
        .output()
        .expect("the runner starts");
    assert!(
        !out.status.success(),
        "a reference to a connection that does not exist is not a valid pipeline: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
