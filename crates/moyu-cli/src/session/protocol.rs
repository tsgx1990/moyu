//! Wire types of the `moyu session` stdio protocol (the GUI-shell channel).
//!
//! Framing: newline-delimited JSON in both directions. Every stdout object
//! carries `"v"` ([`crate::output::SCHEMA_VERSION`]) plus a `"type"`
//! discriminator: `hello` | `receipt` | `event` | `fatal` (see
//! `super::writer`). Commands arrive one JSON object per stdin line as
//! [`Incoming`]. stdout stays PURE JSONL — every diagnostic goes to stderr.
//!
//! A `receipt` answers exactly one command (correlated by the echoed `id`);
//! an `event` is asynchronous (produced by the sync pump) and never carries
//! an `id`. Routing rule for the driving process: `type=="receipt"` ⇒ match
//! it to a pending command; `type=="event"` ⇒ fan out by its inner `event`
//! key. Per-command failure is an `ok:false` receipt — the session stays
//! alive (unlike the one-shot `--json` contract, where `ok:false` exits 1).

use serde::Deserialize;

/// Version of the session protocol itself (independent of the `"v"` payload
/// schema version): bump on any breaking change to framing, the state
/// machine, or a command's argument contract.
pub(crate) const PROTOCOL_VERSION: u64 = 1;

/// One command from the driving process, one JSON object per stdin line:
/// `{"id":"c17","cmd":"send","args":{...}}`.
#[derive(Deserialize)]
pub(crate) struct Incoming {
    /// Caller-chosen opaque correlation id, echoed verbatim on the receipt.
    /// Optional so a human driving a session by hand still gets replies —
    /// but a GUI should always set it.
    #[serde(default)]
    pub id: Option<String>,
    pub cmd: String,
    /// Per-command argument object (shape documented per command in
    /// `super::dispatch`); defaults to `null` for argument-less commands.
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Stable machine slugs for `ok:false` receipts. A GUI branches on these,
/// NEVER on `error` text (which is human-facing and free to change).
#[derive(Clone, Copy, Debug)]
pub(crate) enum ErrCode {
    /// No account exists yet — only `init`/`join`/`whoami` are valid.
    NoAccount,
    /// The session holds no unlocked engine — `unlock` first.
    Locked,
    /// The supplied passphrase failed to open the account. Retry is fine
    /// (the session stays alive; repeated failures back off linearly).
    BadPassphrase,
    /// `cmd` is not in this session's `capabilities`.
    UnknownCmd,
    /// The command line wasn't valid JSON, or `args` didn't match the
    /// command's contract.
    InvalidArgs,
    /// A named target (group, workspace, contact, …) resolved to nothing.
    NotFound,
    /// The underlying engine/relay/store operation failed.
    Engine,
}

impl ErrCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ErrCode::NoAccount => "no_account",
            ErrCode::Locked => "locked",
            ErrCode::BadPassphrase => "bad_passphrase",
            ErrCode::UnknownCmd => "unknown_cmd",
            ErrCode::InvalidArgs => "invalid_args",
            ErrCode::NotFound => "not_found",
            ErrCode::Engine => "engine",
        }
    }
}
