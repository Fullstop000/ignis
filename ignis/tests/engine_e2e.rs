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

/// Startup resume: when the Ink launcher forwards a resolved session id via
/// `IGNIS_SESSION_ID`, the engine must (a) adopt that id and (b) replay its
/// persisted transcript at launch so the frontend paints prior history — the
/// parity the engine path lacked (`ignis --resume <id>` under Ink showed an
/// empty conversation). Seeds a session on disk, then asserts a `transcript`
/// frame carrying its messages arrives before the snapshot. No provider needed:
/// the replay reads disk and emits before any turn.
#[test]
fn engine_replays_forwarded_session_transcript_at_startup() {
    let home = TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".ignis")).unwrap();
    let proj = TempDir::new().unwrap();
    // Canonicalize: the engine derives its sessions dir from `current_dir()`,
    // which is the physical path (macOS resolves the `/var` → `/private/var`
    // symlink). Seeding under the raw `proj.path()` would land in a different
    // slug and the replay would silently find nothing — flaky only on macOS.
    let proj_dir = proj.path().canonicalize().unwrap();

    // Persist a session in the project's sessions dir (same path the engine
    // resolves from `HOME` + cwd), in the JSONL record shape `load_session` reads.
    let sessions_dir = ignis::session::project_sessions_dir(&home.path().join(".ignis"), &proj_dir);
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let id = "session-resume-fixture";
    let start_dir = proj_dir.to_string_lossy();
    let jsonl = format!(
        concat!(
            r#"{{"type":"session_meta","timestamp":1,"payload":{{"id":"{id}","start_dir":"{dir}"}}}}"#,
            "\n",
            r#"{{"type":"message","timestamp":2,"payload":{{"role":"user","content":"remember APRICOT"}}}}"#,
            "\n",
            r#"{{"type":"message","timestamp":3,"payload":{{"role":"assistant","content":"noted: APRICOT"}}}}"#,
            "\n"
        ),
        id = id,
        dir = start_dir,
    );
    std::fs::write(sessions_dir.join(format!("{id}.jsonl")), jsonl).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ignis"))
        .arg("--engine")
        .current_dir(&proj_dir)
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .env("IGNIS_SESSION_ID", id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ignis --engine");

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // Scan the first few frames for the transcript replay (it precedes the
    // snapshot, but don't over-fit the exact order).
    let mut transcript = None;
    for _ in 0..5 {
        let line = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("engine emitted a frame within 20s");
        let v: serde_json::Value = serde_json::from_str(&line).expect("frame is valid JSON");
        if v["kind"] == "transcript" {
            transcript = Some(v);
            break;
        }
    }
    let t = transcript.expect("engine replays a transcript frame for the forwarded session");
    assert_eq!(
        t["data"]["session_id"], id,
        "transcript adopts the forwarded id"
    );
    let blocks = t["data"]["blocks"]
        .as_array()
        .expect("transcript carries blocks");
    let rendered = serde_json::to_string(blocks).unwrap();
    assert!(
        rendered.contains("remember APRICOT") && rendered.contains("noted: APRICOT"),
        "replayed transcript carries the persisted turn, got: {rendered}"
    );

    drop(stdin);
    let _ = reader.join();
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    assert!(status.is_some(), "engine exits after stdin closes");
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
