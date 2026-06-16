//! End-to-end test for `ignis --engine` — the headless protocol engine
//! (PR #174, topology ii). Drives the real binary over plain stdin/stdout
//! pipes (no PTY): a `ClientCommand::Submit` line in must produce an `Outbound`
//! event line out, proving the NDJSON protocol round-trips across the process
//! boundary exactly as the in-process port does.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;

/// Write a `submit` over the engine's stdin and assert an `Outbound` event line
/// comes back on stdout. The temp HOME has no provider, so the agent fails to
/// build one and emits `TurnEnd` — which is still a real protocol frame proving
/// the round-trip (Submit → agent → AgentEvent → StdioPort → stdout).
#[test]
fn engine_round_trips_a_submit_over_stdio() {
    let home = TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".ignis")).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ignis"))
        .arg("--engine")
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ignis --engine");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // Read lines on a thread so the test can time out instead of hanging.
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // The engine greets a fresh frontend with a session snapshot (statusline
    // meta) before any turn.
    let first = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("engine emitted an Outbound line within 20s");
    let snap: serde_json::Value =
        serde_json::from_str(&first).expect("Outbound line is valid JSON");
    assert_eq!(
        snap["kind"], "snapshot",
        "first frame is a snapshot, got: {first}"
    );
    assert!(
        snap["data"]["session_id"].is_string() && snap["data"]["cwd"].is_string(),
        "snapshot carries session meta, got: {first}"
    );

    stdin
        .write_all(b"{\"kind\":\"submit\",\"data\":{\"text\":\"hi\"}}\n")
        .unwrap();
    stdin.flush().unwrap();

    // The submit then produces a valid, kind-tagged event frame.
    let line = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("engine emitted an event within 20s");
    let v: serde_json::Value = serde_json::from_str(&line).expect("Outbound line is valid JSON");
    assert_eq!(v["kind"], "event", "submit produces an event, got: {line}");
    assert!(
        v["data"]["type"].is_string(),
        "event carries a typed AgentEvent, got: {line}"
    );

    // Closing stdin (EOF) is a clean disconnect — the engine exits promptly.
    drop(stdin);
    let _ = reader.join();
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    assert!(
        status.is_some(),
        "engine exits after its stdin (the protocol channel) closes"
    );
}

/// `child.wait()` has no timeout; poll `try_wait` so a hung child fails the test
/// instead of wedging it.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
