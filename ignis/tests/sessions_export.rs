//! End-to-end test for `ignis sessions export --html`. Spawns the release
//! binary against a tempdir HOME with a fixture session, asserts the HTML
//! file is produced and contains the expected row.

use std::process::Command;

fn ignis_bin() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // workspace root
    p.push("target");
    p.push(if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    });
    p.push("ignis");
    p
}

#[test]
fn export_writes_html_with_session_row() {
    if !ignis_bin().exists() {
        eprintln!("skipping: {} not built", ignis_bin().display());
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cwd = tmp.path().join("workdir");
    let projects = home.join(".ignis/projects");
    let slug = ignis::session::project_slug(&cwd);
    let proj = projects.join(&slug);
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    let jsonl = format!(
        "{}\n{}\n",
        r#"{"type":"session_meta","timestamp":1735787045,"payload":{"id":"sess-it","start_dir":"/tmp/x"}}"#,
        r#"{"type":"message","timestamp":1735787046,"payload":{"role":"user","content":"hi"}}"#,
    );
    std::fs::write(proj.join("sess-it.jsonl"), jsonl).unwrap();
    std::fs::write(
        proj.join("sess-it.usage.json"),
        r#"{"input_tokens":42,"output_tokens":7}"#,
    )
    .unwrap();

    let out = cwd.join("report.html");
    let status = Command::new(ignis_bin())
        .arg("sessions")
        .arg("export")
        .arg("--html")
        .arg("--scope")
        .arg("current")
        .arg("--output")
        .arg(&out)
        .env("HOME", &home)
        .current_dir(&cwd)
        .status()
        .unwrap();
    assert!(status.success(), "exit code: {status}");
    assert!(out.exists(), "html file not written");

    let html = std::fs::read_to_string(&out).unwrap();
    assert!(html.contains("sess-it"), "expected session id in html");
    // input + output = 42 + 7 = 49 (Usage::total semantics)
    assert!(
        html.contains(">49<"),
        "expected total tokens (49) in summary card"
    );
}
