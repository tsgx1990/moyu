//! strfry `writePolicy` plugin: reads one JSON request per line on stdin, emits
//! one `{"id","action","msg"}` per line on stdout. See strfry's plugin docs.
//! All decision logic is in `policy::decide` (unit-tested); this is just the
//! transport shell + the real clock.

mod policy;

use std::io::{BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

#[derive(Deserialize)]
struct Req {
    #[serde(rename = "type")]
    kind: String,
    event: Event,
    #[serde(rename = "sourceInfo", default)]
    source_info: String,
}

#[derive(Deserialize)]
struct Event {
    id: String,
    #[serde(default)]
    pubkey: String,
    kind: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut state = policy::RateState::default();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let req: Req = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue, // ignore malformed lines; strfry retries connection-level
        };
        // strfry only asks about newly-submitted events ("new"); accept others.
        let (action, msg) = if req.kind != "new" {
            ("accept", "")
        } else {
            match policy::decide(
                req.event.kind,
                &req.event.pubkey,
                &req.source_info,
                now_secs(),
                &mut state,
            ) {
                policy::Decision::Accept => ("accept", ""),
                policy::Decision::Reject(m) => ("reject", m),
            }
        };
        let out = serde_json::json!({ "id": req.event.id, "action": action, "msg": msg });
        if writeln!(stdout, "{out}").is_err() {
            break;
        }
        let _ = stdout.flush();
    }
}
