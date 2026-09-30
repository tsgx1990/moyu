//! The session's single stdout writer: one locked `writeln!` per JSON object
//! (an event can never splice into the middle of a receipt line), write
//! errors swallowed (broken pipe = the GUI died; the reader loop notices via
//! stdin EOF) — the same discipline as `crate::output::emit`, but with the
//! session's `"type"` framing instead of the one-shot bare objects.
//! Deliberately NOT `output::emit` itself: the one-shot `--json` error path
//! in `main()` emits bare `{"v":1,"ok":false}` objects, which would corrupt
//! the framed stream (`session::run` therefore never returns `Err`).

use std::io::Write;

use crate::output::SCHEMA_VERSION;

use super::protocol::{ErrCode, PROTOCOL_VERSION};

pub(crate) struct SessionWriter;

impl SessionWriter {
    fn emit(&self, value: &serde_json::Value) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{value}");
    }

    /// Emitted once at startup, before any command is read: protocol
    /// version, lock state, account presence, and the exact command set this
    /// build dispatches (a GUI feature-gates on `capabilities`, so the list
    /// must only ever name commands that really work).
    pub(crate) fn hello(&self, state: &str, account: serde_json::Value, capabilities: &[&str]) {
        self.emit(&serde_json::json!({
            "v": SCHEMA_VERSION,
            "type": "hello",
            "protocol": PROTOCOL_VERSION,
            "agent_version": env!("CARGO_PKG_VERSION"),
            "state": state,
            "account": account,
            "capabilities": capabilities,
        }));
    }

    pub(crate) fn receipt_ok(&self, id: Option<&str>, data: serde_json::Value) {
        self.emit(&serde_json::json!({
            "v": SCHEMA_VERSION,
            "type": "receipt",
            "id": id,
            "ok": true,
            "data": data,
        }));
    }

    pub(crate) fn receipt_err(&self, id: Option<&str>, code: ErrCode, error: &str) {
        self.emit(&serde_json::json!({
            "v": SCHEMA_VERSION,
            "type": "receipt",
            "id": id,
            "ok": false,
            "code": code.as_str(),
            "error": error,
        }));
    }

    /// Wrap one async [`crate::ops::SessionEvent`] JSON object (which already
    /// carries `"v"` and `"event"`) in the session frame.
    pub(crate) fn event(&self, mut inner: serde_json::Value) {
        if let Some(obj) = inner.as_object_mut() {
            obj.insert("type".to_owned(), serde_json::json!("event"));
        }
        self.emit(&inner);
    }

    /// Unrecoverable end of session: one final framed line, then the caller
    /// exits. `stdin_closed` (the parent went away / closed us) is the
    /// orderly variant.
    pub(crate) fn fatal(&self, code: &str, error: &str) {
        self.emit(&serde_json::json!({
            "v": SCHEMA_VERSION,
            "type": "fatal",
            "code": code,
            "error": error,
        }));
    }
}
