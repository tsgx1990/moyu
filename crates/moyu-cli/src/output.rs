//! Structured (`--json`) output plumbing for the programmable / bot-facing CLI
//! surface ("moyu speaks JSON / pipes / streams").
//!
//! A single process-global switch, set once from the global `--json` flag in
//! `main`, is chosen over threading a `json: bool` through every command
//! handler: a CLI invocation runs in exactly one output mode for its whole
//! lifetime, so a global is faithful to the model and keeps the (already
//! wide) handler signatures unchanged.
//!
//! # JSON contract (stable — bots depend on it)
//!
//! Every emitted object carries a schema version `"v"` ([`SCHEMA_VERSION`]);
//! bump it on any breaking change so consumers can gate.
//!
//! - **receipts** (one-shot `send`/`post` confirmations):
//!   `{"v":1,"ok":true, ...}`
//! - **errors** (any top-level failure while `--json` is set):
//!   `{"v":1,"ok":false,"error":"..."}` on stdout, exit code 1.
//! - **stream events** (`recv`): one JSON object per line (JSONL),
//!   discriminated by `"event"`: `"message"` / `"joined"` / `"governance"` /
//!   `"snapshot_rebroadcast"`.
//! - **list commands** (`workspace list`, `channel list`): a single JSON array
//!   of row objects, each row carrying `"v"`.
//!
//! `"ok"` is a **mutation/error** marker: it is `true` on a `send`/`post`
//! receipt and `false` on an error. Read commands (`whoami` and the lists)
//! return their data directly and deliberately carry no `"ok"` -- a consumer
//! keys success off a zero exit code + valid JSON, and `"ok"` off receipts.
//!
//! # Coverage boundary
//!
//! `--json` now covers the full one-shot command surface: the bot-loop
//! commands (`whoami`, `workspace list`, `channel list`, `send`, `post`,
//! `recv`, `search`, `react`, `reply`, `download`, `op`, `activity`, `invite`,
//! `join`, `requests`, `approve`, `deny`, `relay list|forget`) AND the
//! one-shot setup/admin mutations (`init`, `add`, `keypackage publish|rotate`,
//! `workspace new|add|members|rename|kick|admin list|add|remove|leave`,
//! `channel new|new-private|invite|list|rename|archive`) -- every `cmd_*`
//! body lives print-free in `ops.rs` (see that module's doc comment) with a
//! typed receipt rendered here or at the call site. A consumer should parse
//! stdout as JSON for any of these commands. The interactive `chat`/`tui`
//! front ends are the only holdouts -- they ignore `--json` entirely (they
//! are not bot surfaces, there is no single receipt to emit for a REPL).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// Schema version stamped on every structured object as `"v"`. Increment on any
/// breaking change to a field's presence, name, or meaning.
pub const SCHEMA_VERSION: u64 = 1;

static JSON_MODE: AtomicBool = AtomicBool::new(false);

/// Set the process-wide output mode. Called once from `main` right after arg
/// parsing, before any command runs.
pub fn set_json_mode(on: bool) {
    JSON_MODE.store(on, Ordering::Relaxed);
}

/// Whether `--json` was requested. Handlers branch on this to pick a
/// machine-readable object over human text.
pub fn json_mode() -> bool {
    JSON_MODE.load(Ordering::Relaxed)
}

/// Print one compact JSON value as a single line (JSONL) to stdout.
///
/// Uses a locked `writeln!` and deliberately **ignores** write errors rather
/// than `println!`, which panics with an ugly "failed printing to stdout:
/// Broken pipe" when a streaming consumer (`moyu recv --follow --json | head`)
/// closes the pipe. Silently ceasing to write is the least-surprising
/// behaviour here; a clean broken-pipe *exit* is a separate follow-up.
pub fn emit(value: &serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
}

/// Stamp a receipt body with the schema version and `"ok":true`, then emit
/// it -- the sequence most one-shot `cmd_*` mutation handlers repeat
/// verbatim after building their receipt: `body["v"] = SCHEMA_VERSION;
/// body["ok"] = true; emit(&body)`. Insertion order matters here (a
/// `serde_json::Map` is order-preserving, and `"v"`/`"ok"` are absent from
/// every receipt's own `to_json()`, so both calls always *append*): callers
/// that need extra fields after `"ok"` (e.g. an `"event"` tag) must keep
/// stamping inline rather than using this helper, or the wire order shifts.
pub fn emit_ok(mut body: serde_json::Value) {
    body["v"] = serde_json::json!(SCHEMA_VERSION);
    body["ok"] = serde_json::json!(true);
    emit(&body);
}

/// Stamp `"v"` onto every row of a list-returning `cmd_*` handler and emit
/// the whole list as one bare JSON array line -- mirrors the sequence each
/// `cmd_*_list`/`cmd_search`/`cmd_requests`/... handler repeats: collect each
/// row's own `to_json()`, stamp `"v"` on it (appended, same reasoning as
/// [`emit_ok`]), collect into a `Vec`, emit as `Value::Array`.
pub fn emit_rows(rows: impl IntoIterator<Item = serde_json::Value>) {
    let stamped: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|mut row| {
            row["v"] = serde_json::json!(SCHEMA_VERSION);
            row
        })
        .collect();
    emit(&serde_json::Value::Array(stamped));
}

// ---------------------------------------------------------------------------
// Terminal-safe human output.
//
// Everything printed for a human (message bodies, sender/workspace/channel
// names, invite/relay strings, MDK error text) ultimately comes from another
// user or a relay -- none of it is trusted. Without sanitizing, a malicious
// group member could plant a terminal escape (retitle the window, an OSC 52
// clipboard write, an OSC 8 link whose visible text lies about its target,
// cursor movement that hides text) or a Unicode bidi override that visually
// reorders a line, in a message body or display name and have it executed by
// every terminal that renders `moyu recv`/`moyu history`/etc.
//
// The fix lives here, at the two points where a *human* line is actually
// written (`human_line`/`human_err_line`, reached only through the
// `hprintln!`/`heprintln!` macros) -- not scattered across every call site
// that builds one. `--json` (`emit`/`emit_ok`/`emit_rows`) and the session
// JSONL stream (`crate::session::writer::SessionWriter`) are a machine
// contract and stay byte-lossless; they must never route through here.
// ---------------------------------------------------------------------------

/// Whether `c` must be dropped or replaced before a human line is safe to
/// write to a terminal. Shared between the "does this need an allocation at
/// all" fast path and the actual rewrite in [`term_safe`].
fn is_unsafe_for_terminal(c: char) -> bool {
    match c {
        // Deleted separately (turns "\r\n" into "\n", drops a lone "\r"):
        // never reaches this predicate for the replace-with-U+FFFD path, but
        // listed here too so the "does this line need touching at all" scan
        // in `term_safe` catches it.
        '\r' => true,
        // Preserved: multi-line message bodies and the odd literal tab.
        '\n' | '\t' => false,
        // C0 (incl. ESC/BS) and DEL, plus C1 (incl. CSI/OSC at U+009B/U+009D).
        c if c.is_control() => true,
        // Bidi controls: ALM, LRM/RLM, the LRE/RLE/PDF/LRO/RLO block, and the
        // LRI/RLI/FSI/PDI isolates -- all capable of visually reordering or
        // relabeling surrounding text.
        '\u{061c}'
        | '\u{200e}'
        | '\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2066}'..='\u{2069}' => true,
        // Deprecated interlinear annotation format characters, and the
        // Unicode tag block (historically usable to smuggle invisible
        // payloads inside an otherwise-plain emoji/flag sequence).
        '\u{fff9}'..='\u{fffb}' | '\u{e0000}'..='\u{e007f}' => true,
        _ => false,
    }
}

/// Sanitize one line/blob of untrusted remote-origin text for display on a
/// human terminal.
///
/// - every `\r` is deleted (so `"\r\n"` becomes `"\n"` and a lone `\r`
///   vanishes rather than moving the cursor back to column 0);
/// - every other control character (`char::is_control`) is replaced with
///   U+FFFD, **except** `\n` and `\t` -- this covers C0 (including ESC and
///   backspace), DEL, and C1 (including CSI/OSC, which do not start with the
///   ASCII ESC byte but with a single C1 byte);
/// - Unicode bidi control characters (U+061C, U+200E, U+200F,
///   U+202A..=U+202E, U+2066..=U+2069) are replaced with U+FFFD;
/// - the interlinear-annotation format characters U+FFF9..=U+FFFB and the
///   Unicode tag block U+E0000..=U+E007F are replaced with U+FFFD.
///
/// Everything else is kept verbatim, including ZWJ (U+200D) and ZWNJ
/// (U+200C) -- required by emoji ZWJ sequences and by Persian/Indic scripts.
///
/// Returns `Cow::Borrowed` when nothing needed changing (the overwhelming
/// common case for ordinary chat text), so the hot path of every human print
/// does not pay for an allocation it doesn't need.
pub fn term_safe(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(is_unsafe_for_terminal) {
        return std::borrow::Cow::Borrowed(s);
    }

    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\r' => {}
            c if is_unsafe_for_terminal(c) => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Indent the continuation lines of a multi-line message BODY so an embedded
/// `\n` can't spoof a following, independently-attributed line of chat
/// output. Without this, a body like `"hi\n[deadbeef] mallory … [#general]:
/// fake"` renders flush against the left margin and reads as a second,
/// unrelated message. Every line after the first gets a four-space prefix;
/// the first line -- printed right after the caller's own
/// sender/label/timestamp prefix -- is left untouched.
///
/// Apply this to the message body only, never to names/labels (those are
/// already length-bounded/sanitized elsewhere and indenting them would be
/// misleading).
///
/// Returns `Cow::Borrowed` when there is no `\n` (the overwhelming common
/// case), so single-line bodies pay no allocation cost. A trailing `\n` is
/// preserved as-is, with no indent after it (nothing follows it to spoof).
pub(crate) fn indent_continuation(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\n') {
        return std::borrow::Cow::Borrowed(s);
    }

    let trailing_newline = s.ends_with('\n');
    let body = if trailing_newline {
        &s[..s.len() - 1]
    } else {
        s
    };

    let mut out = String::with_capacity(s.len() + 16);
    let mut lines = body.split('\n');
    if let Some(first) = lines.next() {
        out.push_str(first);
    }
    for line in lines {
        out.push_str("\n    ");
        out.push_str(line);
    }
    if trailing_newline {
        out.push('\n');
    }
    std::borrow::Cow::Owned(out)
}

/// Write one human-readable line to stdout, sanitized by [`term_safe`].
/// Ignores write errors -- the same broken-pipe discipline as [`emit`] --
/// and writes via a locked `writeln!` (never `println!`/`print!`) so this
/// function is exempt from the crate's `#![deny(clippy::print_stdout)]` by
/// construction, no `#[allow]` needed.
///
/// Reached only through the [`crate::hprintln!`] macro -- use that at call
/// sites, not this function directly.
pub fn human_line(args: std::fmt::Arguments<'_>) {
    let text = args.to_string();
    let safe = term_safe(&text);
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{safe}");
}

/// `stderr` counterpart of [`human_line`] -- see there. Reached only through
/// [`crate::heprintln!`].
pub fn human_err_line(args: std::fmt::Arguments<'_>) {
    let text = args.to_string();
    let safe = term_safe(&text);
    let mut out = std::io::stderr().lock();
    let _ = writeln!(out, "{safe}");
}

/// `eprint!` (no trailing newline) counterpart of [`human_err_line`], for an
/// interactive prompt immediately followed by a `read_line` on stdin. Flushes
/// stderr itself so callers don't need their own `use std::io::Write` just
/// for that. Sanitized by [`term_safe`] like every other human write, even
/// though today's only callers are static prompt strings -- consistent
/// behavior beats a special case. Reached only through [`crate::heprint!`].
pub fn human_err_prompt(args: std::fmt::Arguments<'_>) {
    let text = args.to_string();
    let safe = term_safe(&text);
    let mut out = std::io::stderr().lock();
    let _ = write!(out, "{safe}");
    let _ = out.flush();
}

/// `println!`, but the formatted line is passed through [`term_safe`]
/// before being written, so remote-origin text (message bodies, sender /
/// workspace / channel names, error strings) can't smuggle terminal escapes
/// or bidi overrides into a human-facing line. Use this everywhere a CLI
/// command prints for a human; `--json` output and the session JSONL stream
/// are a machine contract and go through `output::emit`/`SessionWriter`
/// instead, which must stay byte-lossless.
macro_rules! hprintln {
    ($($arg:tt)*) => {
        $crate::output::human_line(::std::format_args!($($arg)*))
    };
}

/// `eprintln!` counterpart of [`hprintln!`] -- see there.
macro_rules! heprintln {
    ($($arg:tt)*) => {
        $crate::output::human_err_line(::std::format_args!($($arg)*))
    };
}

/// `eprint!` (no trailing newline) counterpart of [`hprintln!`], for an
/// interactive prompt immediately followed by a `read_line` on stdin.
macro_rules! heprint {
    ($($arg:tt)*) => {
        $crate::output::human_err_prompt(::std::format_args!($($arg)*))
    };
}

#[cfg(test)]
mod term_safe_tests {
    use super::term_safe;
    use std::borrow::Cow;

    fn assert_borrowed(s: &str) {
        match term_safe(s) {
            Cow::Borrowed(b) => assert_eq!(b, s),
            Cow::Owned(o) => panic!("expected Cow::Borrowed for {s:?}, got owned {o:?}"),
        }
    }

    #[test]
    fn plain_ascii_is_borrowed_unchanged() {
        assert_borrowed("hello world, moyu!");
    }

    #[test]
    fn cjk_is_borrowed_unchanged() {
        assert_borrowed("你好，摸鱼群组");
    }

    #[test]
    fn emoji_with_zwj_is_borrowed_unchanged() {
        // Family emoji: man + ZWJ + woman + ZWJ + girl + ZWJ + boy.
        assert_borrowed("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}");
    }

    #[test]
    fn zwnj_is_borrowed_unchanged() {
        // ZWNJ as used in Persian/Indic scripts (e.g. "می‌خواهم").
        assert_borrowed("می\u{200C}خواهم");
    }

    #[test]
    fn esc_csi_becomes_replacement_char() {
        let out = term_safe("hello \x1b[31mred\x1b[0m");
        assert_eq!(out, "hello \u{fffd}[31mred\u{fffd}[0m");
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn osc52_clipboard_write_is_neutralized() {
        let out = term_safe("\x1b]52;c;aGVsbG8=\x07");
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\x07'));
        assert_eq!(out, "\u{fffd}]52;c;aGVsbG8=\u{fffd}");
    }

    #[test]
    fn c1_csi_becomes_replacement_char() {
        let out = term_safe("hi\u{9b}31mthere");
        assert_eq!(out, "hi\u{fffd}31mthere");
    }

    #[test]
    fn rlo_becomes_replacement_char() {
        let out = term_safe("safe\u{202e}evil");
        assert_eq!(out, "safe\u{fffd}evil");
    }

    #[test]
    fn lri_becomes_replacement_char() {
        let out = term_safe("safe\u{2066}evil\u{2069}");
        assert_eq!(out, "safe\u{fffd}evil\u{fffd}");
    }

    #[test]
    fn tag_chars_become_replacement_char() {
        let out = term_safe("flag\u{e0041}\u{e0042}");
        assert_eq!(out, "flag\u{fffd}\u{fffd}");
    }

    #[test]
    fn crlf_becomes_lf() {
        let out = term_safe("hello\r\nworld");
        assert_eq!(out, "hello\nworld");
    }

    #[test]
    fn lone_cr_is_deleted() {
        let out = term_safe("hello\rworld");
        assert_eq!(out, "helloworld");
    }

    #[test]
    fn tab_and_newline_are_preserved() {
        assert_borrowed("col1\tcol2\nrow2a\trow2b");
    }
}

#[cfg(test)]
mod indent_continuation_tests {
    use super::indent_continuation;
    use std::borrow::Cow;

    #[test]
    fn no_newline_is_borrowed_unchanged() {
        match indent_continuation("just one line") {
            Cow::Borrowed(b) => assert_eq!(b, "just one line"),
            Cow::Owned(o) => panic!("expected Cow::Borrowed, got owned {o:?}"),
        }
    }

    #[test]
    fn multiple_lines_get_four_space_continuation_indent() {
        let out = indent_continuation("a\nb\nc");
        assert_eq!(out, "a\n    b\n    c");
    }

    /// A trailing `\n` is preserved as-is with no dangling indent after it --
    /// there is no following line left to spoof, so nothing needs indenting.
    #[test]
    fn trailing_newline_has_no_dangling_indent() {
        let out = indent_continuation("a\nb\n");
        assert_eq!(out, "a\n    b\n");
    }

    /// Guards against the exact spoof this function exists to prevent: an
    /// embedded fake "[id] sender [#chan]:" line stays indented, not
    /// flush-left where it could be mistaken for a real message.
    #[test]
    fn spoofed_following_line_stays_indented() {
        let out = indent_continuation("hi\n[abcd1234] alice [#general]: fake");
        assert_eq!(out, "hi\n    [abcd1234] alice [#general]: fake");
        assert!(!out.starts_with("hi\n["));
    }
}
