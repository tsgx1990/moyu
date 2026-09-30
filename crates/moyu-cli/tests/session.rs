//! Integration tests for `moyu session`'s framed stdio protocol — the
//! hermetic half: state-machine refusals, framing discipline, and clean EOF,
//! all without a relay or a real account (every invocation points at a
//! CLOSED loopback relay so an accidental dial fails fast locally and no
//! test traffic can ever reach a public relay). The live half — pump events
//! over a real relay — lives in `scripts/e2e-local.sh`'s session block.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A fresh scratch data dir under the OS temp dir (no tempfile dependency;
/// the OS reaps temp). Unique per test via name + pid.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("moyu-session-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Spawn `moyu session`, write `commands` (one per line), close stdin, and
/// collect the parsed stdout frames. The closed-port relay guarantees
/// hermeticity (see module doc).
fn drive_session(data_dir: &Path, commands: &[&str]) -> Vec<serde_json::Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_moyu"))
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--relay",
            "ws://127.0.0.1:1",
            "--dev-allow-loopback",
            "session",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Captured (on a drain thread, so a full pipe can't deadlock the
        // child) purely for failure forensics: a session that dies at
        // startup writes its reason to stderr, and swallowing it turns a
        // one-run CI diagnosis into a guessing game.
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn moyu session");

    {
        let mut stdin = child.stdin.take().unwrap();
        for cmd in commands {
            writeln!(stdin, "{cmd}").unwrap();
        }
        // Dropping stdin sends EOF: the session replies to everything queued,
        // then emits the fatal frame and exits.
    }

    let stderr_pipe = child.stderr.take().unwrap();
    let stderr_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let mut r = BufReader::new(stderr_pipe);
        let _ = r.read_to_string(&mut buf);
        buf
    });

    let stdout = BufReader::new(child.stdout.take().unwrap());
    let frames: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| {
            let l = l.unwrap();
            serde_json::from_str(&l).unwrap_or_else(|e| panic!("unframed stdout line {l:?}: {e}"))
        })
        .collect();
    let status = child.wait().unwrap();
    let stderr = stderr_thread.join().unwrap_or_default();
    assert!(
        status.success(),
        "session exited non-zero: {status:?}\n--- child stderr ---\n{stderr}\n--- frames ({}) ---\n{frames:#?}",
        frames.len(),
    );
    frames
}

/// Every stdout object must carry the schema version and a type frame.
fn assert_framing(frames: &[serde_json::Value]) {
    for f in frames {
        assert_eq!(f["v"], 1, "missing/wrong schema version: {f}");
        assert!(f["type"].is_string(), "missing type frame: {f}");
    }
    assert_eq!(frames.first().unwrap()["type"], "hello");
    let last = frames.last().unwrap();
    assert_eq!(last["type"], "fatal");
    assert_eq!(last["code"], "stdin_closed");
}

fn receipt<'a>(frames: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
    frames
        .iter()
        .find(|f| f["type"] == "receipt" && f["id"] == id)
        .unwrap_or_else(|| panic!("no receipt {id}"))
}

#[test]
fn no_account_state_machine_and_framing() {
    let dir = scratch_dir("no-account");
    let frames = drive_session(
        &dir,
        &[
            r#"{"id":"w1","cmd":"whoami"}"#,
            r#"{"id":"s1","cmd":"send","args":{"peer":"npub1x","message":"hi"}}"#,
            r#"{"id":"u1","cmd":"unlock","args":{"passphrase":"x"}}"#,
            r#"not json at all"#,
            r#"{"id":"z1","cmd":"nosuchcmd"}"#,
            r#"{"id":"r1","cmd":"relay_list"}"#,
        ],
    );
    assert_framing(&frames);

    let hello = &frames[0];
    assert_eq!(hello["state"], "no-account");
    assert_eq!(hello["account"]["present"], false);
    let caps: Vec<&str> = hello["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for must in ["unlock", "send", "catchup", "init", "join", "history"] {
        assert!(caps.contains(&must), "capability {must} missing");
    }

    assert_eq!(receipt(&frames, "w1")["data"]["present"], false);
    assert_eq!(receipt(&frames, "s1")["code"], "no_account");
    assert_eq!(receipt(&frames, "u1")["code"], "no_account");
    assert_eq!(receipt(&frames, "z1")["code"], "unknown_cmd");
    // relay_list is valid in any state (engine-less local read).
    assert_eq!(receipt(&frames, "r1")["ok"], true);
    // The garbage line gets an uncorrelated invalid_args receipt.
    let bad = frames
        .iter()
        .find(|f| f["type"] == "receipt" && f["code"] == "invalid_args")
        .expect("invalid_args receipt");
    assert!(bad["id"].is_null());
}

#[test]
fn locked_state_refusals_and_bad_passphrase() {
    let dir = scratch_dir("locked");
    // Fabricate a locked account: an active-account pointer whose account
    // data doesn't exist. Unlock must fail as bad_passphrase (any first
    // decrypt/open failure maps there), never crash the session.
    moyu_core::store::write_active_account_label(&dir, &"ab".repeat(32)).unwrap();

    let frames = drive_session(
        &dir,
        &[
            r#"{"id":"h1","cmd":"history","args":{"group":"abcd"}}"#,
            r#"{"id":"u1","cmd":"unlock","args":{"passphrase":"wrong"}}"#,
            r#"{"id":"u2","cmd":"unlock","args":{}}"#,
            r#"{"id":"l1","cmd":"lock"}"#,
            r#"{"id":"w1","cmd":"whoami"}"#,
            r#"{"id":"i1","cmd":"init","args":{"passphrase":"x"}}"#,
        ],
    );
    assert_framing(&frames);

    assert_eq!(frames[0]["state"], "locked");
    assert_eq!(frames[0]["account"]["present"], true);
    assert_eq!(receipt(&frames, "h1")["code"], "locked");
    assert_eq!(receipt(&frames, "u1")["code"], "bad_passphrase");
    assert_eq!(receipt(&frames, "u2")["code"], "invalid_args");
    // lock is idempotent from locked.
    assert_eq!(receipt(&frames, "l1")["data"]["state"], "locked");
    assert_eq!(receipt(&frames, "w1")["data"]["present"], true);
    // init refuses when an account already exists.
    assert_eq!(receipt(&frames, "i1")["ok"], false);

    let _ = std::fs::remove_dir_all(&dir);
}
