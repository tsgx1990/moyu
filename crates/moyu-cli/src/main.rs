//! moyu-cli: the front end for moyu, an E2EE CLI chat tool over
//! MLS-over-Nostr (MDK / Marmot). Two front ends over the same headless core:
//! the scriptable subcommands (`chat`'s rustyline REPL, `send`/`recv`, ...) and
//! a full-screen ratatui UI via `moyu tui` (`crate::tui`). Every command below is written directly against
//! `moyu_core`'s public API, never `marmot-app`/`marmot-account`/
//! `cgka-traits` directly, keeping the headless-core / front-end split.
//!
//! # Status
//!
//! Built and exercised end-to-end: `cargo test --workspace` is green and
//! `scripts/e2e-local.sh` proves a 1:1 MLS message round-trips and decrypts
//! on both sides over a local relay. Written against MDK source read directly
//! (see `../../docs/mdk-api-map.md`).
//!
//! # Terminal-safe output
//!
//! Everything printed for a human ultimately traces back to another user or
//! a relay (message bodies, sender/workspace/channel names, invite/relay
//! strings, MDK error text) and is therefore untrusted: `#![deny(...)]`
//! below bans raw `println!`/`eprintln!` crate-wide so a future call site
//! can't reintroduce an unsanitized print. Use `hprintln!`/`heprintln!`
//! (from `output`, in scope everywhere via `#[macro_use]` below) instead --
//! they route every line through `output::term_safe` first. `--json`
//! (`output::emit`/`emit_ok`/`emit_rows`) and the session JSONL stream
//! (`session::writer::SessionWriter`) are a machine contract, stay
//! byte-lossless, and are exempt because they write via `writeln!` on a
//! locked handle rather than through these macros.
#![deny(clippy::print_stdout, clippy::print_stderr)]

#[macro_use]
mod output;
mod domain;
mod ops;
mod relay_select;
mod repl;
mod session;
mod tui;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use moyu_core::engine::{AppClient, MoyuEngine};
use moyu_core::invite;
use moyu_core::store::MoyuStore;
use moyu_core::transport;

// Everything main.rs still calls directly out of the domain layer
// (crates/moyu-cli/src/domain/) -- the extracted shared core `ops.rs` and
// `session/mod.rs` also depend on. Listed explicitly (no glob) so this import
// block IS the contract of what main.rs actually uses.
use domain::{
    AdminChange, RECV_MAX_IDLE_POLLS, RECV_MAX_TOTAL_POLLS, RECV_POLL_INTERVAL, active_label,
    build_engine, find_or_create_dm, refresh_keypackage, reject_loopback_relays,
    resolve_group_hex_offline, snapshot_rosters, trust_badge,
};

#[derive(Parser)]
#[command(
    name = "moyu",
    version,
    about = "moyu -- E2EE CLI chat over MLS/Nostr (M0 skeleton)"
)]
struct Cli {
    /// Override the data directory (default: OS-standard app-data dir).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Relay to use (repeatable). Defaults to a few public relays. For local
    /// testing point it at a self-hosted relay, e.g.
    /// `--relay ws://127.0.0.1:7777` (also requires `--dev-allow-loopback`).
    #[arg(long = "relay", global = true)]
    relays: Vec<String>,

    /// Allow loopback relays (`127.0.0.0/8`, `::1`, `localhost`). DEV/TESTING
    /// ONLY -- required to point `--relay` at a local relay (e.g. a
    /// self-hosted nostr-rs-relay for the E2E harness). Off by default:
    /// without it a loopback `--relay` is refused up front. Never use loopback
    /// relays in production.
    #[arg(long = "dev-allow-loopback", global = true)]
    dev_allow_loopback: bool,

    /// Route all relay connections, NIP-05 lookups and attachment transfers
    /// through a SOCKS5 proxy at IP:PORT (e.g. `--socks5 127.0.0.1:1080` for
    /// a local Tor or `ssh -D` SOCKS port -- an IP literal, not a hostname).
    /// Needed on a network where relays are not directly reachable. Off by
    /// default (dialed directly).
    #[arg(long = "socks5", global = true, value_name = "IP:PORT")]
    socks5: Option<SocketAddr>,

    /// Emit machine-readable JSON instead of human text, for scripts and bots.
    /// One-shot commands print a single JSON object (a receipt / a list array);
    /// `recv` streams one JSON object per line (JSONL). Every object carries a
    /// schema version `"v"`. On failure a `{"v":..,"ok":false,"error":..}`
    /// object is printed and the process exits non-zero. See `src/output.rs`.
    #[arg(long, global = true)]
    json: bool,

    /// Blossom blob server to upload attachments to (`post`/`send --file`),
    /// overriding the group's default (`https://blossom.primal.net`). Blobs are
    /// end-to-end encrypted (the key never leaves the MLS group), but the server
    /// still sees ciphertext size/timing. For local testing:
    /// `--blossom http://127.0.0.1:PORT` together with `--dev-allow-loopback`.
    #[arg(long = "blossom", global = true, value_name = "URL")]
    blossom: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new identity (or `--import-nsec` an existing one) and publish
    /// its first KeyPackage.
    Init {
        /// Import an existing identity's secret key (`nsec1...` or 64 hex
        /// chars) instead of generating a new one. `--import-nsec` with no
        /// value prompts for the key without echo (or reads one line from
        /// stdin when piped); passing the key inline is discouraged (shell
        /// history, `ps`).
        #[arg(
            long,
            num_args = 0..=1,
            default_missing_value = "-",
            value_name = "NSEC"
        )]
        import_nsec: Option<String>,
        /// Pick the relay set non-interactively: `public` (the decentralized
        /// built-in default). Skips the interactive prompt. An explicit
        /// `--relay` still overrides this; there is no hosted preset -- bring
        /// your own relay with `--relay` (see docs/self-host-relay.md).
        #[arg(long = "relay-preset", value_enum)]
        relay_preset: Option<relay_select::RelayPreset>,
    },
    /// Show the active account's label and npub.
    Whoami,
    /// Publish or force-rotate this account's KeyPackage (kind 30443).
    Keypackage {
        #[command(subcommand)]
        action: KeypackageAction,
    },
    /// Add a contact by `npub1...`, hex pubkey, or NIP-05 (`name@domain`).
    Add {
        npub_or_nip05: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// Open (creating if needed) a 1:1 chat with a contact and enter the REPL.
    Chat { peer: String },
    /// Send one message to a peer (creating the 1:1 group first if needed)
    /// and exit. Non-interactive counterpart to `chat`, for scripts/bots.
    Send {
        peer: String,
        /// Message body. Omit it (or pass `-`) to read the whole message from
        /// stdin: `echo "deploy ✅" | moyu send <peer>`. With `--file`, this is
        /// the attachment's caption instead.
        message: Option<String>,
        /// Attach a file (encrypted end-to-end, uploaded to a Blossom blob
        /// server). The positional argument becomes the caption.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
    },
    /// Drain inbound MLS traffic once: accept any pending group invites
    /// (Welcomes), sync, and print any newly-received messages, then exit.
    Recv {
        /// Keep polling forever instead of exiting after the initial drain
        /// settles (Ctrl-C to stop).
        #[arg(long)]
        follow: bool,
    },
    /// Open the full-screen chat UI (ratatui): a chat list + live send/receive
    /// across all your 1:1 chats in one screen. Ctrl-C to quit.
    Tui,
    /// Encrypted multi-member workspaces (persistent circles with channels).
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Channels within a workspace.
    Channel {
        #[command(subcommand)]
        action: ChannelAction,
    },
    /// Post a message to a workspace channel.
    Post {
        ws: String,
        channel: String,
        /// Message body. Omit it (or pass `-`) to read the whole message from
        /// stdin: `echo "deploy ✅" | moyu post <ws> <channel>`. With `--file`,
        /// this is the attachment's caption instead.
        message: Option<String>,
        /// Attach a file (encrypted end-to-end, uploaded to a Blossom blob
        /// server). The positional argument becomes the caption.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
    },
    /// Download an encrypted attachment from a group and decrypt it to a file.
    /// Address it by its content hash (the `ciphertext_sha256` shown in
    /// `recv --json`'s `attachments[]`).
    Download {
        /// A workspace name/prefix, or a group-id hex/prefix (the `group` field
        /// from `recv --json`).
        group: String,
        /// The attachment's `ciphertext_sha256` (or its `plaintext_sha256`).
        hash: String,
        /// Directory or file path to write the decrypted file to. Default: the
        /// current directory, using the attachment's own file name.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// React to a message with an emoji (kind-7 reaction), or retract your
    /// reaction with `--remove`. Address the group by workspace name or by a
    /// group-id hex/prefix, and the message by its `message_id` — both as shown
    /// in `recv --json`. Lets a bot acknowledge a message without a full reply.
    React {
        /// A workspace name/prefix, or a group-id hex/prefix (`recv --json`'s
        /// `group`).
        group: String,
        /// The target message's `message_id` (from `recv --json`).
        message_id: String,
        /// The emoji to react with (e.g. 👍). Required unless `--remove`.
        emoji: Option<String>,
        /// Retract your earlier reaction to this message instead of adding one.
        #[arg(long)]
        remove: bool,
    },
    /// Reply to a message — a first-class threaded reply (kind-9 carrying the
    /// parent's `e`+`q` tags) so the recipient sees what it answers.
    Reply {
        /// A workspace name/prefix, or a group-id hex/prefix (`recv --json`'s
        /// `group`).
        group: String,
        /// The target message's `message_id` (from `recv --json`).
        message_id: String,
        /// Reply body. Omit it (or pass `-`) to read the whole reply from
        /// stdin: `echo "on it 👍" | moyu reply <group> <message_id>`.
        message: Option<String>,
    },
    /// Search your local (already-decrypted) message history for a substring —
    /// a grep over everything moyu has synced, done entirely offline (it reads
    /// the local encrypted store, never a relay). Great for scripts:
    /// `moyu search "deploy" --json`.
    Search {
        /// Case-insensitive substring to look for in message bodies.
        query: String,
        /// Restrict to one group by its group-id hex/prefix (`recv --json`'s
        /// `group`). Omit to search across every group.
        #[arg(long)]
        group: Option<String>,
        /// Cap the number of matches returned (most recent first).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Read one group's local message history, newest page first (offline --
    /// reads the local encrypted store, never a relay). Page backwards with
    /// `--before <next_cursor>`. For scripts/GUIs: `moyu history 4b37 --json`.
    History {
        /// Group-id hex, or a unique prefix of one -- find it via
        /// `conversations`, `workspace list`, or `recv --json`'s `group`.
        group: String,
        /// Opaque cursor from a previous page's `next_cursor`: return the
        /// next OLDER page.
        #[arg(long)]
        before: Option<String>,
        /// Rows per page (the newest `limit` of the selected range).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// List every conversation in one read: DMs + workspaces (channels nested,
    /// 🔒 private included) + saved contacts with no chat yet. A GUI/bot
    /// hydrates its sidebar from `moyu conversations --json`.
    Conversations,
    /// Long-lived stdio command/event loop for GUI shells: JSON commands in
    /// on stdin, JSONL receipts + async events out on stdout (framed by a
    /// `type` key; see the session protocol docs). Starts locked -- the
    /// driving process sends an `unlock` command, never a tty prompt.
    Session,
    /// Emit a bot/agent OPERATION event (kind-1202) into a workspace channel —
    /// a structured CI/deploy/git/monitoring event marked as bot activity at the
    /// protocol level (not a human kind-9 message). Built for integrations:
    /// `moyu op ops ci --type ci --status failed --name build --fail "3 tests failed"`
    /// or pipe the summary on stdin (`ci-log | moyu op ops ci --type ci --status ok`).
    Op {
        /// Workspace name/prefix.
        ws: String,
        /// Channel slug within the workspace (public, or a private channel you're in).
        channel: String,
        /// Human summary line. Omit (or pass `-`) to read from stdin; may be
        /// empty — the structured fields below carry the meaning.
        text: Option<String>,
        /// Operation category (required): e.g. ci, deploy, git-push, monitor.
        #[arg(long = "type", value_name = "TYPE")]
        event_type: String,
        /// Operation status (required): e.g. success, failed, running, queued.
        #[arg(long)]
        status: String,
        /// A short name for the job/operation (e.g. the CI job name).
        #[arg(long)]
        name: Option<String>,
        /// A correlation id (e.g. the CI run id) grouping related events.
        #[arg(long = "run-id")]
        run_id: Option<String>,
        /// Mark the operation as succeeded (sets `ok:true`).
        #[arg(long, conflicts_with = "fail")]
        ok: bool,
        /// Mark the operation as failed (sets `ok:false`).
        #[arg(long)]
        fail: bool,
        /// Elapsed time in milliseconds.
        #[arg(long = "duration-ms")]
        duration_ms: Option<u64>,
        /// A short preview string (e.g. the first failing line).
        #[arg(long)]
        preview: Option<String>,
        /// Arbitrary JSON object merged into the event's `details`.
        #[arg(long)]
        details: Option<String>,
    },
    /// Post a bot/agent ACTIVITY line (kind-1201) into a workspace channel — a
    /// lightweight "bot said/did X" message, marked as bot activity at the
    /// protocol level so it renders with a 🤖 marker (unlike a plain `post`,
    /// which looks like a human). For simple notifications:
    /// `moyu activity ops ci "deploy to prod started by @alice"`.
    Activity {
        /// Workspace name/prefix.
        ws: String,
        /// Channel slug within the workspace (public, or a private channel you're in).
        channel: String,
        /// The bot message. Omit (or pass `-`) to read it from stdin.
        text: Option<String>,
        /// The agent's status (e.g. active, working, done, blocked).
        #[arg(long, default_value = "active")]
        status: String,
        /// Arbitrary JSON object merged into the event's `extra`.
        #[arg(long)]
        extra: Option<String>,
    },
    /// Print an invite code. No <workspace> = a 1:1 contact invite; with a
    /// workspace slug = a "join my workspace" invite (requires you be an admin
    /// to fulfill). stdout prints ONLY the code (diagnostics on stderr) so
    /// `moyu join "$(moyu invite eng)"` works. A workspace code is a bearer
    /// token -- anyone holding it can ask to join -- and stops working 7 days
    /// after it is issued.
    Invite {
        workspace: Option<String>,
        /// Add whoever presents this code without a manual `approve`. Someone
        /// who was removed from the workspace is never re-admitted this way.
        #[arg(long = "auto-approve")]
        auto_approve: bool,
        /// Instead of issuing a code, cancel every code already issued for
        /// <workspace>: they stop being trusted or auto-approved. People who
        /// already joined are not affected.
        #[arg(long, requires = "workspace", conflicts_with = "auto_approve")]
        revoke: bool,
    },
    /// Join via an invite code (`moyuinv1...`, or any link that carries one
    /// after a `#`). Configures
    /// relays, ensures your identity exists + KeyPackage is published, then
    /// (contact) DMs the inviter, or (workspace) sends a join request.
    Join { code: String },
    /// List pending workspace join requests (from `moyu join`), deduped by
    /// sender and hiding anyone already a member. Badges each as ✓ trusted
    /// (it presents a secret you actually issued) or ⚠ uncredentialed.
    Requests,
    /// Approve a pending join request: add the sender to the workspace
    /// (admin only) and broadcast a catch-up `WorkspaceSnapshot`. Pass a
    /// npub/hex, or `all` to approve every pending request.
    Approve {
        who: String,
        /// Only requests for this workspace. Required when the same person
        /// has asked to join more than one.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Dismiss a pending join request locally (no membership granted, no
    /// event sent -- the requester's own retry or `join` fixes it).
    Deny { who: String },
    /// Inspect or prune the persisted relay set (`config.json`). `join` unions
    /// an inviter's relays into this set, so `relay list` audits what you're
    /// connected to and `relay forget <url>` drops one you'd rather not use.
    Relay {
        #[command(subcommand)]
        action: RelayAction,
    },
}

#[derive(Subcommand)]
enum KeypackageAction {
    Publish,
    Rotate,
}

/// `relay …` — inspect/prune the persistent relay set in `config.json` (the
/// union `init`/`join` build up). Local file ops only; no account/relay needed.
#[derive(Subcommand)]
enum RelayAction {
    /// List the relays persisted in config.json (one per line on stdout).
    List,
    /// Add one relay (`wss://…`) to the persisted set -- e.g. your own
    /// self-hosted relay (docs/self-host-relay.md). Idempotent.
    Add { url: String },
    /// Remove one relay from the persisted set by exact URL.
    Forget { url: String },
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// Create a new workspace (a fresh MLS group with just you in it).
    New { name: String },
    /// List every workspace this account belongs to.
    List,
    /// Invite a member into a workspace by npub, hex pubkey, or NIP-05.
    Add { ws: String, npub_or_nip05: String },
    /// List a workspace's members (admins are badged).
    Members { ws: String },
    /// Rename a workspace.
    Rename { ws: String, name: String },
    /// Remove a member from a workspace (admin only). Member = npub / hex / NIP-05.
    Kick { ws: String, member: String },
    /// Manage a workspace's admin set (admin only).
    Admin {
        #[command(subcommand)]
        action: AdminAction,
    },
    /// Leave a workspace. If you are its only admin, pass `--transfer-to`
    /// to hand admin to another member first.
    Leave {
        ws: String,
        /// Member (npub / hex / NIP-05) to promote to admin before leaving —
        /// required when you are the workspace's only admin.
        #[arg(long)]
        transfer_to: Option<String>,
    },
}

/// `workspace admin …` — the binary admin set (MDK enforces admin-only on
/// invite/remove/rename; see the M2 governance design spec).
#[derive(Subcommand)]
enum AdminAction {
    /// List a workspace's admins.
    List { ws: String },
    /// Promote a member to admin (admin only). Member = npub / hex / NIP-05.
    Add { ws: String, member: String },
    /// Demote a member from admin (admin only). Member = npub / hex / NIP-05.
    Remove { ws: String, member: String },
}

#[derive(Subcommand)]
enum ChannelAction {
    /// Create a new (public) channel in a workspace.
    New { ws: String, name: String },
    /// Create a PRIVATE channel: a separate MLS group nested under the workspace,
    /// visible only to the members you invite (who must already be in the
    /// workspace). Fully hidden from other workspace members.
    NewPrivate {
        ws: String,
        name: String,
        /// Members to invite (npub / hex / NIP-05). Each must already be a
        /// member of the parent workspace.
        members: Vec<String>,
    },
    /// Invite a member (npub / hex / NIP-05) into an existing private channel.
    /// They must already be a member of the parent workspace.
    Invite {
        ws: String,
        slug: String,
        member: String,
    },
    /// List a workspace's channels (public, then 🔒 private ones you're in).
    List { ws: String },
    /// Rename a channel.
    Rename {
        ws: String,
        slug: String,
        name: String,
    },
    /// Archive a channel.
    Archive { ws: String, slug: String },
}

fn default_data_dir() -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("chat", "moyu", "moyu").ok_or_else(|| {
        anyhow::anyhow!("could not determine a home/app-data directory for this OS/user")
    })?;
    Ok(dirs.data_dir().to_path_buf())
}

/// Read a passphrase without echoing it.
///
/// For scripting / CI (and local E2E tests), `MOYU_PASSPHRASE` overrides the
/// interactive prompt when set and non-empty. Env vars are readable via
/// `ps -E` / `/proc`, so this is a deliberate convenience for automation, not
/// the recommended path for real use — interactive no-echo `rpassword` input
/// (which never hits the screen or shell scrollback) stays the default.
fn prompt_passphrase(label: &str) -> anyhow::Result<String> {
    if let Ok(p) = std::env::var("MOYU_PASSPHRASE")
        && !p.is_empty()
    {
        return Ok(p);
    }
    Ok(rpassword::prompt_password(format!("{label}: "))?)
}

/// Resolve `--import-nsec`'s raw clap value into the secret to import.
///
/// The flag takes `num_args = 0..=1` with `default_missing_value = "-"`, so:
/// not passed at all -> `None`; passed bare (or explicitly `-`) -> read the
/// key out-of-band (no-echo prompt on a tty, one stdin line when piped) so it
/// never sits in argv/shell history/`ps`; passed with an inline value ->
/// that value, still accepted but discouraged (see the flag's help text).
/// The result is wrapped in `Zeroizing` as early as possible.
/// The relays in `offered` that are not already in `known`, in order, each
/// once.
fn relays_new_to<'a>(known: &[String], offered: &'a [String]) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for r in offered {
        if !known.iter().any(|k| k == r) && !out.contains(&r.as_str()) {
            out.push(r);
        }
    }
    out
}

fn resolve_import_nsec(arg: Option<String>) -> anyhow::Result<Option<zeroize::Zeroizing<String>>> {
    let value = match arg {
        None => return Ok(None),
        Some(v) => v,
    };
    if value != "-" {
        return Ok(Some(zeroize::Zeroizing::new(value)));
    }
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        let mut nsec =
            zeroize::Zeroizing::new(rpassword::prompt_password("nsec (input hidden): ")?);
        trim_in_place(&mut nsec);
        if nsec.is_empty() {
            anyhow::bail!("no nsec provided");
        }
        Ok(Some(nsec))
    } else {
        let stdin = std::io::stdin();
        Ok(Some(read_nsec_line(stdin.lock())?))
    }
}

/// Read exactly one line from `r`, trimmed of the trailing `\n`/`\r\n` --
/// factored out of `resolve_import_nsec` so the non-tty (piped) path is
/// unit-testable without a real stdin. Errors on an empty line so a caller
/// piping nothing (or a blank line) gets an actionable message instead of
/// silently importing an empty "secret".
fn read_nsec_line(mut r: impl std::io::BufRead) -> anyhow::Result<zeroize::Zeroizing<String>> {
    // Read straight into the zeroizing buffer and trim *in place*: a plain
    // `String` + `trim().to_owned()` would leave an un-wiped copy of the key
    // on the heap (the whole point of the piped path is that no such copy
    // outlives the import). Pre-sized for the same reason: a `String` that
    // grows frees its old, smaller buffer without wiping it, and a key
    // arriving in two short reads would leave its first half behind. 256
    // bytes covers both the bech32 (63) and hex (64) forms with room to
    // spare. (std's own stdin buffer still holds the line; that one is out
    // of reach.)
    let mut line = zeroize::Zeroizing::new(String::with_capacity(256));
    r.read_line(&mut line)?;
    trim_in_place(&mut line);
    if line.is_empty() {
        anyhow::bail!("no nsec provided");
    }
    Ok(line)
}

/// Strip leading and trailing ASCII whitespace (spaces, tabs, `\r`, `\n`)
/// without allocating, so a secret never gets a second, un-wiped copy:
/// `pop` and `drain` both work inside the existing buffer.
fn trim_in_place(s: &mut String) {
    while s.ends_with(|c: char| c.is_ascii_whitespace()) {
        s.pop();
    }
    let lead = s.len()
        - s.trim_start_matches(|c: char| c.is_ascii_whitespace())
            .len();
    s.drain(..lead);
}

/// Print the `init` relay menu and read a 1/2/3 choice from stdin. Defaults to
/// the public option on empty/unrecognized input (safe, decentralized default).
fn prompt_init_relay_choice() -> relay_select::InitRelayPrompt {
    heprintln!("Choose your relay:");
    heprintln!("  1) Public relays (decentralized default: damus / nos.lol / primal;");
    heprintln!("                    may drop Marmot events)");
    heprintln!("  2) Custom / self-host  (enter wss:// URLs; see README \"Relays\")");
    heprint!("Relay [1/2, default 1]: ");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    match line.trim() {
        "2" => relay_select::InitRelayPrompt::Custom,
        _ => relay_select::InitRelayPrompt::Public,
    }
}

/// Read one or more whitespace/comma-separated `wss://` URLs from stdin for the
/// "custom" relay choice. Empty input falls back to the public default.
fn prompt_custom_relay_urls() -> Vec<String> {
    heprint!("Enter wss:// relay URL(s), space-separated: ");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    let urls: Vec<String> = line
        .split([' ', ',', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if urls.is_empty() {
        transport::default_relays()
    } else {
        urls
    }
}

/// Resolve a message body that may come from a positional argument or from
/// stdin. An explicit argument always wins. A missing argument reads stdin only
/// when it is piped/redirected; the explicit sentinel `-` forces reading stdin
/// even from a tty. So a bot can `echo "deploy ✅" | moyu post ops ci` or
/// `moyu post ops ci < file`, while a human who simply forgot the message on an
/// interactive terminal gets a fast, helpful error instead of a silent hang.
///
/// Note: the passphrase prompt reads the tty (`rpassword`) or `MOYU_PASSPHRASE`,
/// never stdin, so consuming stdin here does not collide with authentication.
fn read_message_arg(arg: Option<String>) -> anyhow::Result<String> {
    match arg {
        Some(m) if m != "-" => return Ok(m),
        // Explicit `-`: the caller asked for stdin; honor it even on a tty.
        Some(_) => {}
        // No argument: reading a tty to EOF would hang silently with no prompt.
        // Only read stdin when it's actually piped; otherwise fail
        // fast with a hint. (`message` used to be a required positional, so a
        // missing one was a clean usage error -- preserve that ergonomics.)
        None => {
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "no message given -- pass it as an argument, pipe it on stdin \
                     (`echo hi | moyu ...`), or use `-` to force reading stdin"
                );
            }
        }
    }
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    normalize_stdin_message(&buf)
        .ok_or_else(|| anyhow::anyhow!("empty message: no argument given and stdin was empty"))
}

/// Like [`read_message_arg`] but tolerates an empty/absent body, returning `""`
/// instead of erroring. Used by `op`/`activity`, whose structured fields
/// (type/status/name/...) carry the meaning, so the free-text summary is
/// optional. `-` still forces reading stdin; an absent arg on a tty is simply
/// empty (never a silent hang), while a piped body is read as usual.
fn read_optional_message_arg(arg: Option<String>) -> anyhow::Result<String> {
    match arg {
        Some(m) if m != "-" => return Ok(m),
        // Explicit `-`: force stdin even from a tty.
        Some(_) => {}
        None => {
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                return Ok(String::new());
            }
        }
    }
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(normalize_stdin_message(&buf).unwrap_or_default())
}

/// Strip the single trailing newline (`\n` or `\r\n`) that a shell
/// `echo`/heredoc appends, preserving any message-internal newlines. Returns
/// `None` when nothing meaningful remains (so the caller can reject an empty
/// send rather than posting a blank line).
fn normalize_stdin_message(raw: &str) -> Option<String> {
    let s = raw.strip_suffix('\n').unwrap_or(raw);
    let s = s.strip_suffix('\r').unwrap_or(s);
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Parse an optional `--details`/`--extra` style JSON flag up front so a bad
/// payload fails before a session opens (and before anything publishes).
fn parse_json_flag(value: Option<&str>, flag: &str) -> anyhow::Result<Option<serde_json::Value>> {
    value
        .map(|s| {
            serde_json::from_str::<serde_json::Value>(s)
                .with_context(|| format!("{flag} must be valid JSON"))
        })
        .transpose()
}

/// Active label + standard "Passphrase" prompt + engine — the shared prologue
/// of every command that operates on the existing active account.
fn open_engine(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<(MoyuEngine, String)> {
    let account_label = active_label(data_dir)?;
    let passphrase = prompt_passphrase("Passphrase")?;
    Ok((
        build_engine(data_dir, relays, allow_loopback, socks5, passphrase),
        account_label,
    ))
}

/// Open engine + client + store for the active account (mirrors cmd_send's
/// prologue). Returns them for the caller to operate on. Every
/// workspace/channel/post command shares this same setup.
async fn open_session(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<(MoyuEngine, AppClient, MoyuStore, String)> {
    let (engine, account_label) = open_engine(data_dir, relays, allow_loopback, socks5)?;
    let mut moyu_store = MoyuStore::open(data_dir, &account_label)?;
    let mut client = engine.client(&account_label).await?;
    refresh_keypackage(&mut client, &mut moyu_store).await;
    Ok((engine, client, moyu_store, account_label))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    // Set the output mode before dispatch so even an early failure (e.g. the
    // loopback-relay guard) surfaces as a JSON error object when `--json` is on.
    output::set_json_mode(cli.json);
    if let Err(e) = run(cli).await {
        if output::json_mode() {
            output::emit(&serde_json::json!({
                "v": output::SCHEMA_VERSION,
                "ok": false,
                "error": format!("{e:#}"),
            }));
        } else {
            heprintln!("Error: {e:?}");
        }
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let data_dir = match cli.data_dir {
        Some(d) => d,
        None => default_data_dir()?,
    };

    // 优先级:CLI --relay(一次性覆盖)> config.relays > default_relays()。
    let relays = moyu_core::config::resolve_relays(&cli.relays, &data_dir);

    // Process-wide guard on the resolved relay set (e.g. a self-hosted
    // nostr-rs-relay for the E2E harness needs the explicit opt-in).
    reject_loopback_relays(&relays, cli.dev_allow_loopback)?;
    let allow_loopback = cli.dev_allow_loopback;
    let socks5 = cli.socks5;
    let blossom = cli.blossom.clone();
    let cli_relays = cli.relays.clone();

    match cli.command {
        Command::Init {
            import_nsec,
            relay_preset,
        } => {
            use std::io::IsTerminal;
            let is_tty = std::io::stdin().is_terminal();
            let (init_relays, relay_source) = relay_select::select_init_relays(
                &cli_relays,
                relay_preset,
                is_tty,
                prompt_init_relay_choice,
                prompt_custom_relay_urls,
            );
            // Loopback guard on the *selected* set (a user may enter a dev
            // loopback under choice 3); mirrors the process-wide guard above.
            reject_loopback_relays(&init_relays, allow_loopback)?;
            let import_nsec = resolve_import_nsec(import_nsec)?;
            cmd_init(
                &data_dir,
                &init_relays,
                relay_source,
                allow_loopback,
                socks5,
                import_nsec,
            )
            .await
        }
        Command::Whoami => cmd_whoami(&data_dir),
        Command::Keypackage { action } => {
            cmd_keypackage(&data_dir, &relays, allow_loopback, socks5, action).await
        }
        Command::Add {
            npub_or_nip05,
            label,
        } => cmd_add(&data_dir, socks5, npub_or_nip05, label).await,
        Command::Chat { peer } => cmd_chat(&data_dir, &relays, allow_loopback, socks5, peer).await,
        Command::Send {
            peer,
            message,
            file,
        } => {
            cmd_send(
                &data_dir,
                &relays,
                allow_loopback,
                socks5,
                blossom,
                peer,
                message,
                file,
            )
            .await
        }
        Command::Recv { follow } => {
            cmd_recv(&data_dir, &relays, allow_loopback, socks5, follow).await
        }
        Command::Tui => cmd_tui(&data_dir, &relays, allow_loopback, socks5).await,
        Command::Workspace { action } => {
            cmd_workspace(&data_dir, &relays, allow_loopback, socks5, action).await
        }
        Command::Channel { action } => {
            cmd_channel(&data_dir, &relays, allow_loopback, socks5, action).await
        }
        Command::Post {
            ws,
            channel,
            message,
            file,
        } => {
            cmd_post(
                &data_dir,
                &relays,
                allow_loopback,
                socks5,
                blossom,
                ws,
                channel,
                message,
                file,
            )
            .await
        }
        Command::Download { group, hash, out } => {
            cmd_download(&data_dir, &relays, allow_loopback, socks5, group, hash, out).await
        }
        Command::React {
            group,
            message_id,
            emoji,
            remove,
        } => {
            cmd_react(
                &data_dir,
                &relays,
                allow_loopback,
                socks5,
                group,
                message_id,
                emoji,
                remove,
            )
            .await
        }
        Command::Reply {
            group,
            message_id,
            message,
        } => {
            cmd_reply(
                &data_dir,
                &relays,
                allow_loopback,
                socks5,
                group,
                message_id,
                message,
            )
            .await
        }
        Command::Search {
            query,
            group,
            limit,
        } => cmd_search(&data_dir, query, group, limit),
        Command::History {
            group,
            before,
            limit,
        } => cmd_history(&data_dir, group, before, limit, cli.dev_allow_loopback),
        Command::Conversations => {
            cmd_conversations(&data_dir, &relays, cli.dev_allow_loopback, socks5).await
        }
        Command::Session => {
            session::run(session::SessionOpts {
                data_dir,
                relays,
                allow_loopback,
                socks5,
                blossom,
            })
            .await
        }
        Command::Op {
            ws,
            channel,
            text,
            event_type,
            status,
            name,
            run_id,
            ok,
            fail,
            duration_ms,
            preview,
            details,
        } => {
            cmd_op(
                &data_dir,
                &relays,
                allow_loopback,
                socks5,
                ws,
                channel,
                text,
                event_type,
                status,
                name,
                run_id,
                ok,
                fail,
                duration_ms,
                preview,
                details,
            )
            .await
        }
        Command::Activity {
            ws,
            channel,
            text,
            status,
            extra,
        } => {
            cmd_activity(
                &data_dir,
                &relays,
                allow_loopback,
                socks5,
                ws,
                channel,
                text,
                status,
                extra,
            )
            .await
        }
        Command::Invite {
            workspace,
            auto_approve,
            revoke,
        } => {
            cmd_invite(
                &data_dir,
                &relays,
                allow_loopback,
                socks5,
                workspace,
                auto_approve,
                revoke,
            )
            .await
        }
        Command::Join { code } => cmd_join(&data_dir, &relays, allow_loopback, socks5, code).await,
        Command::Requests => cmd_requests(&data_dir, &relays, allow_loopback, socks5).await,
        Command::Approve { who, workspace } => {
            cmd_approve(&data_dir, &relays, allow_loopback, socks5, who, workspace).await
        }
        Command::Deny { who } => cmd_deny(who),
        Command::Relay { action } => cmd_relay(&data_dir, allow_loopback, action),
    }
}

async fn cmd_init(
    data_dir: &Path,
    relays: &[String],
    relay_source: relay_select::RelaySource,
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    import_nsec: Option<zeroize::Zeroizing<String>>,
) -> anyhow::Result<()> {
    let passphrase = prompt_passphrase("Choose a passphrase to encrypt your identity at rest")?;
    if passphrase.is_empty() {
        anyhow::bail!("passphrase must not be empty");
    }
    let imported = import_nsec.is_some();
    let receipt = ops::init(
        data_dir,
        relays,
        allow_loopback,
        socks5,
        passphrase,
        import_nsec,
    )
    .await?;

    if output::json_mode() {
        let body = receipt.to_json(relays, imported);
        output::emit_ok(body);
        return Ok(());
    }

    hprintln!("Identity ready.");
    hprintln!("  label: {}", receipt.label);
    match (&receipt.npub, &receipt.npub_encode_err) {
        (Some(npub), _) => hprintln!("  npub:  {npub}"),
        (None, err) => hprintln!(
            "  npub:  <could not encode: {}>",
            err.as_deref().unwrap_or("unknown error")
        ),
    }
    match receipt.key_package_bytes {
        Some(bytes) => hprintln!("  published initial KeyPackage ({bytes} bytes, kind 30443)"),
        None => {
            hprintln!("  (no initial KeyPackage published -- run `moyu keypackage publish` next)")
        }
    }
    // Say where the persisted relay set came from, so a user who accepted the
    // default knows their traffic goes to public relays (and how to change it).
    let source = match relay_source {
        relay_select::RelaySource::Public => "public built-in relays",
        relay_select::RelaySource::Custom => "custom relays",
        relay_select::RelaySource::CliFlag => "relays from --relay",
    };
    hprintln!("  relays ({source}): {}", relays.join(", "));
    hprintln!(
        "  (change later with `moyu relay add|forget <wss://...>`; self-hosting: README \"Relays\")"
    );

    Ok(())
}

fn cmd_whoami(data_dir: &Path) -> anyhow::Result<()> {
    let ops::Whoami { label, npub } = ops::whoami(data_dir)?;
    if output::json_mode() {
        output::emit(&serde_json::json!({
            "v": output::SCHEMA_VERSION,
            "label": label,
            "npub": npub,
        }));
        return Ok(());
    }
    hprintln!("label: {label}");
    // `AccountHome::create_nostr_account` sets label = the account's pubkey
    // hex (confirmed, `crates/marmot-account/src/home.rs:142-145`), so this
    // succeeds for every account `moyu init` creates. An imported
    // watch-only/npub-keyed account could in principle use a different label
    // shape, hence a soft failure rather than `?` here.
    //
    // This intentionally does NOT open a `MoyuEngine`/prompt for a
    // passphrase: whoami only needs the locally persisted label, not the
    // decrypted secret key. If a fuller `whoami` (relay-list
    // status, KeyPackage age, signed-out state) is wanted, moyu-core would
    // need a plain `AccountHome::account(&self, label) -> AccountSummary`
    // passthrough (confirmed to exist at `home.rs:190-205`) added to
    // `engine::MoyuEngine` -- left out of the M0 surface since it isn't
    // needed for the M0 happy path.
    match &npub {
        Some(npub) => hprintln!("npub:  {npub}"),
        None => hprintln!("npub:  <label is not a pubkey hex; imported/watch-only account?>"),
    }
    Ok(())
}

async fn cmd_keypackage(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    action: KeypackageAction,
) -> anyhow::Result<()> {
    let (engine, label) = open_engine(data_dir, relays, allow_loopback, socks5)?;
    let mut moyu_store = MoyuStore::open(data_dir, &label)?;

    let receipt = ops::keypackage(&engine, &mut moyu_store, &label, action).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!(
        "Published KeyPackage ({} bytes, kind 30443, ref {}).",
        receipt.bytes,
        receipt.key_package_ref_hex
    );
    Ok(())
}

async fn cmd_add(
    data_dir: &Path,
    socks5: Option<SocketAddr>,
    npub_or_nip05: String,
    label: Option<String>,
) -> anyhow::Result<()> {
    let account_label = active_label(data_dir)?;
    let mut moyu_store = MoyuStore::open(data_dir, &account_label)?;
    let receipt = ops::add(&mut moyu_store, socks5, npub_or_nip05, label).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!("Added contact '{}' ({}).", receipt.label, receipt.npub);
    Ok(())
}

async fn cmd_chat(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    peer: String,
) -> anyhow::Result<()> {
    let (_engine, mut client, mut moyu_store, _account_label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;

    let (peer_npub, group_id) =
        find_or_create_dm(&mut client, &mut moyu_store, &peer, socks5).await?;

    hprintln!("Chatting with {peer_npub} in group {group_id} -- type a message and press Enter.");
    hprintln!("(Ctrl-D to leave the chat.)");

    repl::run_chat_loop(client, group_id, &mut moyu_store).await
}

/// Non-interactive one-shot send: `find_or_create_dm` then a single
/// `AppClient::send`. For scripts / bots / the E2E harness, where a REPL
/// can't be driven.
#[allow(clippy::too_many_arguments)]
async fn cmd_send(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    blossom: Option<String>,
    peer: String,
    message: Option<String>,
    file: Option<PathBuf>,
) -> anyhow::Result<()> {
    // Text path resolves arg/stdin up front (before the tty passphrase prompt);
    // the attachment path uses the positional as an optional caption only and
    // never touches stdin.
    let body = match file {
        None => ops::SendBody::Text(read_message_arg(message)?),
        Some(path) => ops::SendBody::File {
            path,
            caption: message,
        },
    };
    let (_engine, mut client, mut moyu_store, _account_label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;

    let receipt = ops::send(&mut client, &mut moyu_store, &peer, body, socks5, blossom).await?;

    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
    } else {
        match &receipt.attachments {
            Some(parts) => hprintln!(
                "Sent 📎 {} to {} in group {}.",
                parts.names,
                receipt.peer,
                receipt.group
            ),
            None => hprintln!(
                "Sent to {} in group {} (published {} event(s), ids: {:?}).",
                receipt.peer,
                receipt.group,
                receipt.published,
                receipt.message_ids
            ),
        }
    }
    Ok(())
}

/// `react <group> <message_id> [emoji]` / `react ... --remove`: send a kind-7
/// reaction (or retract it) via MDK's `react_to_message` /
/// `unreact_from_message`. The group is resolved by workspace name or group-id
/// hex/prefix; the target is a `message_id` as shown in `recv --json`. Nothing
/// here reads stdin — an emoji is a short positional, so an omitted one is a
/// hard error (not a stdin read) unless `--remove`.
#[allow(clippy::too_many_arguments)]
async fn cmd_react(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    group: String,
    message_id: String,
    emoji: Option<String>,
    remove: bool,
) -> anyhow::Result<()> {
    let emoji = if remove {
        None
    } else {
        let e = emoji.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        if e.is_none() {
            anyhow::bail!(
                "react needs an emoji (e.g. `moyu react <group> <message_id> 👍`), or pass \
                 --remove to retract your reaction"
            );
        }
        e
    };
    let (engine, mut client, _moyu_store, account_label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;

    let receipt = ops::react(
        &engine,
        &mut client,
        &account_label,
        &group,
        &message_id,
        emoji.as_deref(),
    )
    .await?;

    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
    } else {
        let g: String = receipt.group.chars().take(8).collect();
        let t: String = receipt.target.chars().take(8).collect();
        match &emoji {
            Some(e) => hprintln!(
                "Reacted {e} to message {t}… in group {g}… (published {} event(s)).",
                receipt.published
            ),
            None => hprintln!(
                "Retracted reaction to message {t}… in group {g}… (published {} event(s)).",
                receipt.published
            ),
        }
    }
    Ok(())
}

/// `reply <group> <message_id> [text]`: send a first-class threaded reply
/// (kind-9 carrying the parent's `e`+`q` tags) via MDK's `reply_to_message`.
/// The body resolves from the positional arg or stdin exactly like `send`.
async fn cmd_reply(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    group: String,
    message_id: String,
    message: Option<String>,
) -> anyhow::Result<()> {
    // Resolve the reply body (arg or stdin) up front, before the tty prompt.
    let text = read_message_arg(message)?;
    let (engine, mut client, _moyu_store, account_label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;

    let receipt = ops::reply(
        &engine,
        &mut client,
        &account_label,
        &group,
        &message_id,
        text,
    )
    .await?;

    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
    } else {
        let g: String = receipt.group.chars().take(8).collect();
        let t: String = receipt.target.chars().take(8).collect();
        hprintln!(
            "Replied to message {t}… in group {g}… (published {} event(s)).",
            receipt.published
        );
    }
    Ok(())
}

/// `search <query> [--group <g>] [--limit N]`: grep your local (already
/// decrypted) message history. A pure offline local-DB read — it opens the
/// account's SQLCipher store (hence the passphrase) but never a client, so no
/// relay traffic happens. Chat lines only (kind-9, which includes replies and
/// media captions); reactions/edits/deletes/group-system are not text to grep.
/// Results are most-recent-first (by send time), capped at `--limit`.
fn cmd_search(
    data_dir: &Path,
    query: String,
    group: Option<String>,
    limit: usize,
) -> anyhow::Result<()> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        anyhow::bail!("search needs a non-empty query");
    }
    // No relays / proxy / loopback: search never opens a client or syncs, it
    // only reads the local encrypted projection, so the network is untouched.
    let (engine, account_label) = open_engine(data_dir, &[], false, None)?;

    let group_filter = group
        .map(|g| g.trim().to_lowercase())
        .filter(|g| !g.is_empty());
    let hits = ops::search(
        &engine,
        &account_label,
        &needle,
        group_filter.as_deref(),
        limit,
    )?;

    if output::json_mode() {
        output::emit_rows(hits.iter().map(|h| h.to_json()));
        return Ok(());
    }
    if hits.is_empty() {
        // Keep stdout clean (grep-friendly): the "nothing found" note is stderr.
        heprintln!("(no matches for {query:?})");
        return Ok(());
    }
    for h in &hits {
        let g: String = h.group.chars().take(8).collect();
        let s: String = h.sender.chars().take(8).collect();
        let marker = match &h.reply_to {
            Some(parent) => {
                let p: String = parent.chars().take(8).collect();
                format!("↩{p}… ")
            }
            None => String::new(),
        };
        hprintln!(
            "[{g}] {s}… [#{}]: {marker}{}",
            h.channel,
            output::indent_continuation(&h.body)
        );
    }
    Ok(())
}

/// Non-interactive per-group history read (offline, like `search`). The
/// human rendering reuses the live-event line format (`SessionEvent::to_human`)
/// prefixed with a sent/received marker, so history reads like a replayed
/// `recv`.
fn cmd_history(
    data_dir: &Path,
    group: String,
    before: Option<String>,
    limit: usize,
    allow_loopback: bool,
) -> anyhow::Result<()> {
    // No relays / proxy: history never opens a client or syncs, it only
    // reads the local encrypted projection, so the network is untouched.
    let (engine, account_label) = open_engine(data_dir, &[], false, None)?;
    let group_hex = resolve_group_hex_offline(&engine, &account_label, &group)?;
    let page = ops::history(
        &engine,
        &account_label,
        &group_hex,
        before.as_deref(),
        limit,
        allow_loopback,
    )?;

    if output::json_mode() {
        let rows: Vec<serde_json::Value> = page.rows.iter().map(|r| r.to_json()).collect();
        output::emit(&serde_json::json!({
            "v": output::SCHEMA_VERSION,
            "group": page.group,
            "messages": rows,
            "next_cursor": page.next_cursor,
        }));
        return Ok(());
    }
    if page.rows.is_empty() {
        // Keep stdout clean (grep-friendly): the "nothing here" note is stderr.
        heprintln!("(no local history for group {group_hex})");
        return Ok(());
    }
    for row in &page.rows {
        let marker = if row.direction == "sent" {
            "→"
        } else {
            "←"
        };
        hprintln!("{marker} {}", row.event.to_human());
    }
    if let Some(cursor) = &page.next_cursor {
        heprintln!("(older history: --before {cursor})");
    }
    Ok(())
}

/// One-read conversation listing (DMs + workspaces/channels + contacts) — the
/// sidebar view. Opens a client (roster reads identify DM peers) but performs
/// no sync; relay flags are accepted for engine construction parity.
async fn cmd_conversations(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let (engine, account_label) = open_engine(data_dir, relays, allow_loopback, socks5)?;
    let client = engine.client(&account_label).await?;
    let moyu_store = MoyuStore::open(data_dir, &account_label)?;
    let view = ops::conversations(&engine, &client, &moyu_store, &account_label)?;

    if output::json_mode() {
        let mut body = view.to_json();
        body["v"] = serde_json::json!(output::SCHEMA_VERSION);
        output::emit(&body);
        return Ok(());
    }

    if view.dms.is_empty() && view.workspaces.is_empty() && view.contacts.is_empty() {
        hprintln!(
            "no conversations yet -- `moyu add <npub>` then `moyu send`, or `moyu join <code>`"
        );
        return Ok(());
    }
    if !view.dms.is_empty() {
        hprintln!("DMs:");
        for d in &view.dms {
            let g: String = d.group.chars().take(8).collect();
            let who = match (&d.label, &d.npub) {
                (Some(l), Some(n)) => format!("{l} ({n})"),
                (None, Some(n)) => n.clone(),
                _ => "<unknown peer>".to_owned(),
            };
            hprintln!("  {who}  [{g}]");
        }
    }
    if !view.workspaces.is_empty() {
        hprintln!("Workspaces:");
        for w in &view.workspaces {
            let g: String = w.group.chars().take(8).collect();
            let chans: Vec<String> = w
                .channels
                .iter()
                .map(|c| {
                    let lock = if c.private { "🔒" } else { "" };
                    let archived = if c.archived { " (archived)" } else { "" };
                    format!("{lock}#{}{archived}", c.slug)
                })
                .collect();
            hprintln!("  {} [{g}]  {}", w.name, chans.join(" "));
        }
    }
    if !view.contacts.is_empty() {
        hprintln!("Contacts (no chat yet):");
        for c in &view.contacts {
            hprintln!("  {} ({})", c.label, c.npub);
        }
    }
    Ok(())
}

/// Drain inbound MLS/Nostr traffic: accept any pending group invites
/// (Welcomes -- MDK auto-joins the MLS state on ingest, but per
/// `marmot-app`'s own `AGENTS.md`, "Pending invites should stay visible
/// until accepted", so the app-level confirmation is a separate, explicit
/// step here, `AppClient::accept_group_invite`), then print any newly
/// decrypted messages across every local group.
async fn cmd_recv(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    follow: bool,
) -> anyhow::Result<()> {
    let (engine, account_label) = open_engine(data_dir, relays, allow_loopback, socks5)?;
    let mut moyu_store = MoyuStore::open(data_dir, &account_label)?;
    let mut client = engine.client(&account_label).await?;

    // Baseline roster snapshot so the first poll diffs against the on-disk
    // state, not against an empty map (which would announce every existing
    // member as a fresh "joined"). `Option` (see `snapshot_rosters`): a failed
    // read yields `None` and the first diff is simply skipped. Advanced to each
    // loop's post-pass snapshot, but only when that snapshot read cleanly.
    let mut prev_rosters = snapshot_rosters(&engine, &client, &account_label);

    let mut idle_polls = 0usize;
    let mut total_polls = 0usize;
    loop {
        total_polls += 1;
        let progressed = ops::sync_tick(
            &engine,
            &mut client,
            &mut moyu_store,
            &account_label,
            allow_loopback,
            &mut prev_rosters,
            &mut |ev| {
                if output::json_mode() {
                    output::emit(&ev.to_json());
                } else {
                    hprintln!("{}", ev.to_human());
                }
            },
        )
        .await?;

        if follow {
            tokio::time::sleep(RECV_POLL_INTERVAL).await;
            continue;
        }

        idle_polls = if progressed { 0 } else { idle_polls + 1 };
        if idle_polls >= RECV_MAX_IDLE_POLLS || total_polls >= RECV_MAX_TOTAL_POLLS {
            break;
        }
        tokio::time::sleep(RECV_POLL_INTERVAL).await;
    }

    Ok(())
}

/// Open the full-screen ratatui chat UI. Prompts for the passphrase and opens
/// the account (same as `chat`) before taking over the terminal, then hands the
/// live `AppClient` to the TUI event loop.
async fn cmd_tui(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let (engine, client, store, account_label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;
    let mut model = tui::Model::from_store(&account_label, &store, relays, socks5.is_some());
    // Build the workspace/channel tree BEFORE the history backfill below, so
    // the routing that backfill does can land workspace messages in their
    // channel buffers instead of dropping them for lack of a home. Best-effort
    // (same reasoning as the history load): a read failure just starts with
    // no workspaces rather than blocking the UI.
    if let Err(e) = tui::rebuild_workspaces(&mut model, &client, &engine) {
        heprintln!("[workspace tree load failed, starting empty: {e}]");
    }
    // Backfill each chat with its persisted history so opening the TUI shows
    // the conversation, not a blank pane. Best-effort: a read failure just
    // starts empty rather than blocking the UI (printed before the alt-screen).
    match engine.messages(&account_label) {
        Ok(history) => model.load_history(history),
        Err(e) => heprintln!("[history load failed, starting empty: {e}]"),
    }
    tui::run(client, store, model, engine).await
}

async fn cmd_workspace(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    action: WorkspaceAction,
) -> anyhow::Result<()> {
    let (engine, mut client, mut moyu_store, label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;
    match action {
        WorkspaceAction::New { name } => cmd_workspace_new(&mut client, &name).await,
        WorkspaceAction::List => cmd_workspace_list(&engine, &client, &label).await,
        WorkspaceAction::Add { ws, npub_or_nip05 } => {
            cmd_workspace_add(&engine, &mut client, &label, &ws, &npub_or_nip05, socks5).await
        }
        WorkspaceAction::Members { ws } => {
            cmd_workspace_members(&engine, &client, &label, &ws).await
        }
        WorkspaceAction::Rename { ws, name } => {
            cmd_workspace_rename(&engine, &mut client, &label, &ws, &name).await
        }
        WorkspaceAction::Kick { ws, member } => {
            cmd_workspace_kick(
                &engine,
                &mut client,
                &mut moyu_store,
                &label,
                &ws,
                &member,
                socks5,
            )
            .await
        }
        WorkspaceAction::Admin { action } => {
            cmd_workspace_admin(&engine, &mut client, &label, action, socks5).await
        }
        WorkspaceAction::Leave { ws, transfer_to } => {
            cmd_workspace_leave(
                &engine,
                &mut client,
                &label,
                &ws,
                transfer_to.as_deref(),
                socks5,
            )
            .await
        }
    }
}

async fn cmd_channel(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    action: ChannelAction,
) -> anyhow::Result<()> {
    let (engine, mut client, _moyu_store, label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;
    match action {
        ChannelAction::New { ws, name } => {
            cmd_channel_new(&engine, &mut client, &label, &ws, &name).await
        }
        ChannelAction::NewPrivate { ws, name, members } => {
            cmd_channel_new_private(&engine, &mut client, &label, &ws, &name, &members, socks5)
                .await
        }
        ChannelAction::Invite { ws, slug, member } => {
            cmd_channel_invite(&engine, &mut client, &label, &ws, &slug, &member, socks5).await
        }
        ChannelAction::List { ws } => cmd_channel_list(&engine, &client, &label, &ws).await,
        ChannelAction::Rename { ws, slug, name } => {
            cmd_channel_rename(&engine, &mut client, &label, &ws, &slug, &name).await
        }
        ChannelAction::Archive { ws, slug } => {
            cmd_channel_archive(&engine, &mut client, &label, &ws, &slug).await
        }
    }
}

/// `post <ws> <channel> <message>`: opens its own session (it is a top-level
/// command, not a `WorkspaceAction`/`ChannelAction`), resolves the workspace,
/// pre-checks the channel exists (no silent misroute into a channel nobody
/// created), then sends the `{ch,body}`-enveloped chat message.
#[allow(clippy::too_many_arguments)]
async fn cmd_post(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    blossom: Option<String>,
    ws: String,
    channel: String,
    message: Option<String>,
    file: Option<PathBuf>,
) -> anyhow::Result<()> {
    // Text path resolves arg/stdin before the passphrase prompt; the attachment
    // path uses the positional as an optional caption only.
    let body = match file {
        None => ops::PostBody::Text(read_message_arg(message)?),
        Some(path) => ops::PostBody::File {
            path,
            caption: message,
        },
    };
    let (engine, mut client, _moyu_store, label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;

    let receipt = ops::post(&engine, &mut client, &label, &ws, &channel, body, blossom).await?;
    let lock = if receipt.private { "🔒 " } else { "" };

    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
    } else {
        match &receipt.attachments {
            Some(parts) => hprintln!("posted 📎 {} to {lock}#{}.", parts.names, receipt.channel),
            None => hprintln!(
                "posted to {lock}#{} (published {} event(s)).",
                receipt.channel,
                receipt.published
            ),
        }
    }
    Ok(())
}

/// Emit a bot/agent OPERATION event (kind-1202) into a workspace channel —
/// the M5 差异化头牌: a CI/deploy/git/monitoring event marked as bot activity at
/// the protocol level. Mirrors [`cmd_post`]'s workspace/channel resolution
/// (private channel = its own group; public channel = the workspace group after
/// confirming it exists), then sends via the public
/// `AppClient::send_agent_operation_event`. Zero MDK fork.
#[allow(clippy::too_many_arguments)]
async fn cmd_op(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    ws: String,
    channel: String,
    text: Option<String>,
    event_type: String,
    status: String,
    name: Option<String>,
    run_id: Option<String>,
    ok: bool,
    fail: bool,
    duration_ms: Option<u64>,
    preview: Option<String>,
    details: Option<String>,
) -> anyhow::Result<()> {
    let text = read_optional_message_arg(text)?;
    // clap's `conflicts_with` rules out --ok + --fail together, so this maps
    // cleanly to the tri-state `ok: Option<bool>` MDK wants.
    let ok_flag = if ok {
        Some(true)
    } else if fail {
        Some(false)
    } else {
        None
    };
    let details_val = parse_json_flag(details.as_deref(), "--details")?;

    let (engine, mut client, _moyu_store, label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;
    let receipt = ops::op(
        &engine,
        &mut client,
        &label,
        &ws,
        &channel,
        text,
        event_type,
        status,
        name,
        run_id,
        ok_flag,
        duration_ms,
        preview,
        details_val,
    )
    .await?;
    let lock = if receipt.private { "🔒 " } else { "" };
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
    } else {
        hprintln!(
            "🤖 op [{}·{}] → {lock}#{} (published {} event(s)).",
            receipt.event_type,
            receipt.status,
            receipt.channel,
            receipt.published
        );
    }
    Ok(())
}

/// Post a bot/agent ACTIVITY line (kind-1201) into a workspace channel — the
/// lightweight companion to [`cmd_op`]: a plain "bot said/did X" message marked
/// as bot activity (🤖) at the protocol level, so a bot's chatter is visibly a
/// bot's (unlike a plain `post`, which looks like a human). Same channel routing
/// as `post`/`op`; sends via the public `AppClient::send_agent_activity`.
#[allow(clippy::too_many_arguments)]
async fn cmd_activity(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    ws: String,
    channel: String,
    text: Option<String>,
    status: String,
    extra: Option<String>,
) -> anyhow::Result<()> {
    // An activity IS a message, so its body is required (arg or stdin) — unlike
    // `op`, whose structured fields can stand alone.
    let text = read_message_arg(text)?;
    let extra_val = parse_json_flag(extra.as_deref(), "--extra")?;

    let (engine, mut client, _moyu_store, label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;
    let receipt = ops::activity(
        &engine,
        &mut client,
        &label,
        &ws,
        &channel,
        text,
        status,
        extra_val,
    )
    .await?;
    let lock = if receipt.private { "🔒 " } else { "" };
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
    } else {
        hprintln!(
            "🤖 activity [{}] → {lock}#{} (published {} event(s)).",
            receipt.status,
            receipt.channel,
            receipt.published
        );
    }
    Ok(())
}

async fn cmd_invite(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    workspace: Option<String>,
    auto_approve: bool,
    revoke: bool,
) -> anyhow::Result<()> {
    let account_label = active_label(data_dir)?;

    if revoke {
        // clap's `requires` guarantees the slug; keep a real error over a panic.
        let slug = workspace
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--revoke needs a workspace"))?;
        let passphrase = prompt_passphrase("Passphrase")?;
        let engine = build_engine(data_dir, relays, allow_loopback, socks5, passphrase);
        let client = engine.client(&account_label).await?;
        let mut moyu_store = MoyuStore::open(data_dir, &account_label)?;
        let receipt = ops::invite_revoke(&engine, &client, &mut moyu_store, &account_label, slug)?;
        if output::json_mode() {
            output::emit_ok(receipt.to_json());
        } else {
            hprintln!(
                "revoked {} invite code(s) for #{}",
                receipt.revoked,
                receipt.workspace
            );
        }
        return Ok(());
    }

    let receipt = match &workspace {
        None => ops::invite(
            &account_label,
            relays,
            ops::InviteScope::Contact,
            auto_approve,
        )?,
        Some(slug) => {
            let passphrase = prompt_passphrase("Passphrase")?;
            let engine = build_engine(data_dir, relays, allow_loopback, socks5, passphrase);
            let client = engine.client(&account_label).await?;
            let mut moyu_store = MoyuStore::open(data_dir, &account_label)?;
            ops::invite(
                &account_label,
                relays,
                ops::InviteScope::Workspace {
                    engine: &engine,
                    client: &client,
                    store: &mut moyu_store,
                    slug,
                },
                auto_approve,
            )?
        }
    };
    let out = receipt.token.clone();

    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
    } else {
        // stdout 只打码;提示走 stderr,方便 $(moyu invite eng)
        match &workspace {
            Some(w) => heprintln!(
                "workspace invite for #{w}{}: share this code with whoever should join \
                 (valid for 7 days; `moyu invite {w} --revoke` cancels it)",
                if auto_approve {
                    " (auto-approve on)"
                } else {
                    ""
                }
            ),
            None => heprintln!("contact invite: share this code; they run `moyu join <code>`"),
        }
        hprintln!("{out}");
    }
    Ok(())
}

/// Consume an invite code minted by `moyu invite`: decode it, fold its
/// relay(s) into the effective relay set for THIS process AND persist the
/// union to `config.json` (spec C1 -- the engine below MUST be opened with
/// the union, not just the pre-resolved `relays`, or a code pointing at a
/// relay we don't already know about would silently fail to reach the
/// inviter). Inline-inits an identity if this data-dir has none yet (the
/// whole point of `join` is to onboard a brand-new user), ensures our own
/// KeyPackage is published (idempotent -- so the inviter's `approve`/DM-back
/// can always fetch it), then branches: a contact code DMs the inviter a
/// hello; a workspace code DMs the inviter a join-request envelope for them
/// to `moyu approve`.
async fn cmd_join(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    code: String,
) -> anyhow::Result<()> {
    // Accept a bare `moyuinv1...` code, or any link carrying one after a `#`
    // (some chat clients only pass links around; the fragment never leaves the
    // user's machine in an HTTP request).
    let raw = code
        .rsplit_once('#')
        .map(|(_, t)| t)
        .unwrap_or(&code)
        .trim();
    let token = invite::decode_token(raw)?;

    // C1: effective relays = this process's already-resolved relays ∪ the
    // code's relays, unioned into (and persisted to) config.json.
    let mut effective: Vec<String> = relays.to_vec();
    for r in &token.relays {
        if !effective.iter().any(|x| x == r) {
            effective.push(r.clone());
        }
    }
    let effective = moyu_core::config::merge_relays(data_dir, &effective)?;
    // Say which relays the code brought in. They are persisted, so from now
    // on each of them sees this identity connect (pubkey, IP, timing) -- the
    // user should know that, and how to undo it. stderr, so `--json` stdout
    // stays one object.
    for r in relays_new_to(relays, &token.relays) {
        heprintln!(
            "note: the invite code added relay {r} to your relay set; \
             `moyu relay forget {r}` removes it"
        );
    }

    let passphrase = prompt_passphrase("Passphrase to encrypt (or unlock) your identity")?;
    if passphrase.is_empty() {
        anyhow::bail!("passphrase must not be empty");
    }
    let engine = build_engine(data_dir, &effective, allow_loopback, socks5, passphrase);

    let receipt = ops::join(&engine, data_dir, &effective, socks5, token).await?;
    emit_join_receipt(&receipt);
    Ok(())
}

fn emit_join_receipt(receipt: &ops::JoinReceipt) {
    if output::json_mode() {
        let mut body = receipt.to_json();
        body["v"] = serde_json::json!(output::SCHEMA_VERSION);
        body["ok"] = serde_json::json!(true);
        body["event"] = serde_json::json!("join");
        output::emit(&body);
    }
}

/// `requests`: list every pending join request, skipping anyone who is
/// already a member of the target workspace (their request has already been
/// fulfilled -- by this `approve` or another admin's). Each is badged ✓
/// trusted when its secret matches an invite this account actually issued
/// AND was scoped to this same workspace (`scoped_issued_invite`) or ⚠
/// uncredentialed otherwise (e.g. a stale/forwarded/guessed secret, or a
/// leaked secret being replayed against a different workspace) -- `approve`
/// still works either way, the badge is purely advisory.
async fn cmd_requests(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let (engine, account_label) = open_engine(data_dir, relays, allow_loopback, socks5)?;
    let client = engine.client(&account_label).await?;
    let store = MoyuStore::open(data_dir, &account_label)?;

    let rows = ops::requests(&engine, &client, &store, &account_label)?;
    if output::json_mode() {
        output::emit_rows(rows.iter().map(|r| r.to_json()));
        return Ok(());
    }
    for r in &rows {
        let badge = trust_badge(r.trusted);
        hprintln!(
            "🔑 {} 想加入 #{}  [{badge}]  ({})  → moyu approve {}",
            r.npub,
            r.ws_name,
            r.ts,
            r.npub
        );
    }
    Ok(())
}

/// `approve <npub|hex|all>`: add the sender(s) of matching pending join
/// request(s) into their target workspace. `who == "all"` approves every
/// pending request (across every workspace); otherwise only requests from
/// that one npub/hex. Each match is handled by [`approve_one`], which does
/// the actual admin-gated invite + snapshot broadcast.
async fn cmd_approve(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    who: String,
    workspace: Option<String>,
) -> anyhow::Result<()> {
    let (engine, account_label) = open_engine(data_dir, relays, allow_loopback, socks5)?;
    let mut client = engine.client(&account_label).await?;
    let mut store = MoyuStore::open(data_dir, &account_label)?;
    let (outcomes, failures) = ops::approve(
        &engine,
        &mut client,
        &mut store,
        &account_label,
        who,
        workspace.as_deref(),
        false,
    )
    .await?;
    let total = outcomes.len() + failures.len();
    // Rendering identical to when `approve_one` printed inline: an approval
    // is a receipt-like event object / a ✅ line, an already-member no-op is
    // a human-only note (nothing under --json).
    for outcome in outcomes {
        match outcome {
            ops::ApproveOutcome::Approved {
                npub,
                ws_name,
                auto,
            } => {
                if output::json_mode() {
                    // Byte-for-byte the same shape as a live `recv`
                    // `SessionEvent::Approved` event -- reuse its `to_json()`
                    // rather than hand-building the object a second time.
                    output::emit(
                        &ops::SessionEvent::Approved {
                            npub,
                            ws_name,
                            auto,
                        }
                        .to_json(),
                    );
                } else {
                    hprintln!("✅ approved {npub} into #{ws_name}");
                }
            }
            ops::ApproveOutcome::AlreadyMember { npub, ws_name } => {
                if !output::json_mode() {
                    hprintln!("{npub} already in #{ws_name}");
                }
            }
        }
    }
    // Successes above are already rendered (they really happened on the
    // wire) — THEN surface the aggregate failure with the pre-extraction
    // message and exit code.
    if !failures.is_empty() {
        anyhow::bail!(
            "{} of {} approval(s) failed: {}",
            failures.len(),
            total,
            failures.join(", ")
        );
    }
    Ok(())
}

/// `deny <who>`: v1 is a purely local dismissal -- no event is sent, nothing
/// is persisted. The request simply reappears in `moyu requests` until the
/// requester either re-sends it or is approved by another admin; there is no
/// "blocklist" yet. `who` is accepted (and echoed back) for symmetry with
/// `approve`, though this v1 doesn't need it to do anything.
fn cmd_deny(who: String) -> anyhow::Result<()> {
    let receipt = ops::deny(who);
    if output::json_mode() {
        let mut body = receipt.to_json();
        body["v"] = serde_json::json!(output::SCHEMA_VERSION);
        body["ok"] = serde_json::json!(true);
        body["event"] = serde_json::json!("deny");
        output::emit(&body);
    } else {
        heprintln!(
            "dismissed join request from {} (no membership granted)",
            receipt.who
        );
    }
    Ok(())
}

/// `relay list` / `relay forget <url>`: inspect or prune the persistent relay
/// set in `config.json`. Pure local file ops -- no account/passphrase/relay
/// connection -- giving a user a way to audit and undo the relay union that
/// `join` performs from an invite code. Only touches the
/// persisted set; a one-shot `--relay` override is unaffected.
fn cmd_relay(data_dir: &Path, allow_loopback: bool, action: RelayAction) -> anyhow::Result<()> {
    match action {
        RelayAction::Add { url } => {
            // Same guard every other command applies to its relay set: a
            // loopback relay persisted here would otherwise make every later
            // invocation refuse to start until the user finds
            // --dev-allow-loopback.
            reject_loopback_relays(std::slice::from_ref(&url), allow_loopback)?;
            let all = ops::relay_add(data_dir, &url)?;
            if output::json_mode() {
                output::emit(&serde_json::json!({
                    "v": output::SCHEMA_VERSION,
                    "ok": true,
                    "event": "relay_add",
                    "added": url,
                    "relays": all,
                }));
            } else {
                heprintln!("added {url}; {} relay(s) now in config.json", all.len());
            }
        }
        RelayAction::List => {
            let relays = ops::relay_list(data_dir);
            if output::json_mode() {
                output::emit(&serde_json::json!({
                    "v": output::SCHEMA_VERSION,
                    "relays": relays,
                }));
            } else if relays.is_empty() {
                heprintln!("no relays persisted in config.json (using built-in defaults)");
            } else {
                for r in &relays {
                    hprintln!("{r}");
                }
            }
        }
        RelayAction::Forget { url } => {
            let left = ops::relay_forget(data_dir, &url)?;
            if output::json_mode() {
                output::emit(&serde_json::json!({
                    "v": output::SCHEMA_VERSION,
                    "ok": true,
                    "event": "relay_forget",
                    "forgot": url,
                    "relays": left,
                }));
            } else {
                heprintln!(
                    "forgot {url}; {} relay(s) remain in config.json",
                    left.len()
                );
            }
        }
    }
    Ok(())
}

/// `download <group> <hash> [--out]`: find the attachment by content hash in a
/// group's synced messages, `download_media` (fetch from Blossom, decrypt,
/// verify both hashes), and write the plaintext to disk.
async fn cmd_download(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    target: String,
    hash: String,
    out: Option<PathBuf>,
) -> anyhow::Result<()> {
    let (engine, mut client, _moyu_store, label) =
        open_session(data_dir, relays, allow_loopback, socks5).await?;
    let receipt = ops::download(
        &engine,
        &mut client,
        &label,
        &target,
        &hash,
        allow_loopback,
        out,
    )
    .await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
    } else {
        hprintln!(
            "downloaded 📎 {} ({} bytes) -> {}",
            receipt.file_name,
            receipt.size,
            receipt.path.display()
        );
    }
    Ok(())
}

/// `workspace new <name>`: found a brand-new MLS group (no initial invitees --
/// `create_group` supports a solo/empty member list) then immediately send its
/// name as a `WorkspaceRename` control event, which is what makes
/// `resolve_workspace`/`workspace list` recognize it as a workspace at all
/// (`project_workspace(..).name.is_some()`).
async fn cmd_workspace_new(client: &mut AppClient, name: &str) -> anyhow::Result<()> {
    let receipt = ops::workspace_new(client, name).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!(
        "created workspace {} ({})",
        receipt.name,
        &receipt.group[..8.min(receipt.group.len())]
    );
    Ok(())
}

/// `workspace list`: every workspace the account still belongs to (see
/// [`all_workspaces`] -- left/removed groups are excluded, DMs never match).
/// Prints the short group-id, name, channel count, and member count for each.
async fn cmd_workspace_list(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
) -> anyhow::Result<()> {
    let rows = ops::workspace_list(engine, client, label)?;
    if output::json_mode() {
        // Same fail-soft member count as the human path: a failed
        // per-group read is `null`, not an aborted listing.
        output::emit_rows(rows.iter().map(|r| r.to_json()));
        return Ok(());
    }
    if rows.is_empty() {
        hprintln!("no workspaces yet -- create one with `moyu workspace new <name>`");
        return Ok(());
    }
    for r in &rows {
        let short: String = r.group.chars().take(8).collect();
        // A per-group `members()` failure must not abort the whole listing --
        // show that row's member count as `?` and keep going.
        let n_members = r
            .members
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_owned());
        hprintln!(
            "{short}  {}  ({} channel(s), {n_members} member(s))",
            r.name,
            r.channels
        );
    }
    Ok(())
}

/// `workspace add <ws> <npub|nip05>`: invite the peer's MLS KeyPackage into
/// the group, then broadcast a `WorkspaceSnapshot` control event so the new
/// member can catch up on the current name/channel list without needing to
/// replay every historical control message (which MLS forward secrecy means
/// they can't see anyway -- they joined after those events were sent).
async fn cmd_workspace_add(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    peer: &str,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let receipt = ops::workspace_add(engine, client, label, ws, peer, socks5).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!(
        "invited {} into workspace {}",
        receipt.npub,
        receipt.workspace
    );
    Ok(())
}

/// `workspace members <ws>`: list every member's npub, badging admins and the
/// local account. Admins are listed first (see `governance::member_roles`).
async fn cmd_workspace_members(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    ws: &str,
) -> anyhow::Result<()> {
    let rows = ops::workspace_members(engine, client, label, ws)?;
    if output::json_mode() {
        output::emit_rows(rows.iter().map(|r| r.to_json()));
        return Ok(());
    }
    for r in &rows {
        let mut tags: Vec<&str> = Vec::new();
        if r.admin {
            tags.push("admin");
        }
        if r.you {
            tags.push("you");
        }
        let suffix = if tags.is_empty() {
            String::new()
        } else {
            format!(" ({})", tags.join(", "))
        };
        hprintln!("{}{suffix}", r.npub);
    }
    Ok(())
}

/// `workspace rename <ws> <name>`: send a `WorkspaceRename` control event;
/// the projection's `(ts, message_id_hex)` LWW decides the winner if two
/// members race.
async fn cmd_workspace_rename(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    name: &str,
) -> anyhow::Result<()> {
    let receipt = ops::workspace_rename(engine, client, label, ws, name).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!("renamed workspace to {}", receipt.name);
    Ok(())
}

/// `workspace leave <ws> [--transfer-to <member>]`: leave the underlying MLS
/// group. MDK forbids an admin from self-removing while still in the admin set,
/// and forbids emptying the admin set — so the exact steps depend on whether you
/// are an admin and whether a co-admin exists (`governance::leave_plan`):
/// - not an admin → leave directly;
/// - an admin with a co-admin → step down (`self_demote_admin`) then leave;
/// - the SOLE admin → must first promote `--transfer-to <member>`, then step
///   down, then leave (else the group would be left admin-less).
///
/// The steps are separate MLS commits (MDK has no atomic transfer). If one
/// fails mid-way the command reports how far it got and is safe to re-run:
/// `leave_plan` recomputes from the now-current admin set.
async fn cmd_workspace_leave(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    transfer_to: Option<&str>,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let receipt = ops::workspace_leave(engine, client, label, ws, transfer_to, socks5).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!("left workspace {}", receipt.workspace);
    Ok(())
}

/// `workspace kick <ws> <member>`: remove a member from the group (admin only).
/// Pre-checks that you are an admin (nicer than MDK's raw error) and that you
/// are not kicking yourself; MDK is still the authoritative enforcer.
async fn cmd_workspace_kick(
    engine: &MoyuEngine,
    client: &mut AppClient,
    store: &mut MoyuStore,
    label: &str,
    ws: &str,
    member: &str,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let receipt = ops::workspace_kick(engine, client, store, label, ws, member, socks5).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!(
        "removed {} from workspace {}",
        receipt.npub,
        receipt.workspace
    );
    Ok(())
}

/// `workspace admin <list|add|remove>`: manage the group's binary admin set.
async fn cmd_workspace_admin(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    action: AdminAction,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<()> {
    match action {
        AdminAction::List { ws } => cmd_admin_list(engine, client, label, &ws).await,
        AdminAction::Add { ws, member } => {
            cmd_admin_change(
                engine,
                client,
                label,
                &ws,
                &member,
                socks5,
                AdminChange::Promote,
            )
            .await
        }
        AdminAction::Remove { ws, member } => {
            cmd_admin_change(
                engine,
                client,
                label,
                &ws,
                &member,
                socks5,
                AdminChange::Demote,
            )
            .await
        }
    }
}

/// `workspace admin list <ws>`: print the workspace's admins (npub, `(you)` for
/// the local account).
async fn cmd_admin_list(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    ws: &str,
) -> anyhow::Result<()> {
    let listing = ops::admin_list(engine, client, label, ws)?;
    if output::json_mode() {
        output::emit_rows(listing.admins.iter().map(|r| r.to_json()));
        return Ok(());
    }
    if listing.admins.is_empty() {
        hprintln!("workspace {} has no admin set", listing.workspace);
        return Ok(());
    }
    for r in &listing.admins {
        let marker = if r.you { " (you)" } else { "" };
        hprintln!("{}{marker}", r.npub);
    }
    Ok(())
}

/// `workspace admin add|remove <ws> <member>`: promote/demote a member in the
/// admin set (admin only). Pre-checks you are an admin; MDK enforces the rest
/// (e.g. it rejects demoting the last admin, keeping the group governable).
async fn cmd_admin_change(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    member: &str,
    socks5: Option<SocketAddr>,
    change: AdminChange,
) -> anyhow::Result<()> {
    let receipt = ops::admin_change(engine, client, label, ws, member, socks5, change).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    match receipt.action {
        "promote" => hprintln!(
            "promoted {} to admin of {}",
            receipt.npub,
            receipt.workspace
        ),
        _ => hprintln!(
            "demoted {} from admin of {}",
            receipt.npub,
            receipt.workspace
        ),
    }
    Ok(())
}

/// `channel new <ws> <name>`: send a `ChannelCreate` control event. Dedup
/// (two members creating the same slug concurrently) is the projection's
/// concern (`WorkspaceProjection::apply` keeps the first-seen create), so it
/// is always fine to send this even if the channel might already exist.
async fn cmd_channel_new(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    name: &str,
) -> anyhow::Result<()> {
    let receipt = ops::channel_new(engine, client, label, ws, name).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!("created channel #{} ({})", receipt.channel, receipt.name);
    Ok(())
}

/// `channel new-private <ws> <name> [members...]`: create a private channel as a
/// SEPARATE MLS group nested under the workspace. Its `profile.name` encodes the
/// parent workspace id + slug (`workspace::encode_private_channel_name`), so
/// members recognize it the instant they join (no control-message round-trip) and
/// non-members never learn it exists — nothing is announced in the parent group.
/// Every invited member must already belong to the parent workspace (D4:
/// private-channel members ⊆ workspace members; an app-level check, since MLS
/// can't enforce cross-group membership). The creator is its sole initial admin.
async fn cmd_channel_new_private(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    name: &str,
    members: &[String],
    socks5: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let receipt =
        ops::channel_new_private(engine, client, label, ws, name, members, socks5).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!(
        "created private channel 🔒 #{} in workspace {} ({}), invited {} member(s)",
        receipt.channel,
        receipt.workspace,
        &receipt.group[..8.min(receipt.group.len())],
        receipt.invited.len()
    );
    Ok(())
}

/// `channel invite <ws> <slug> <member>`: add a member to an existing private
/// channel. `invite_members` is admin-gated on the private channel's own group,
/// so only that channel's admins can add; the invitee must already be a member of
/// the parent workspace (D4).
async fn cmd_channel_invite(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    slug: &str,
    member: &str,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let receipt = ops::channel_invite(engine, client, label, ws, slug, member, socks5).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!("invited {} into 🔒 #{}", receipt.npub, receipt.channel);
    Ok(())
}

/// `channel list <ws>`: print channels in the projection's stable display
/// order (`#general` first, then by creation time), marking archived ones.
async fn cmd_channel_list(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    ws: &str,
) -> anyhow::Result<()> {
    let rows = ops::channel_list(engine, client, label, ws)?;

    if output::json_mode() {
        output::emit_rows(rows.iter().map(|c| c.to_json()));
        return Ok(());
    }

    for c in &rows {
        if c.private {
            hprintln!("🔒 #{}  {}", c.slug, c.name);
        } else {
            let archived = if c.archived { " (archived)" } else { "" };
            hprintln!("#{}  {}{}", c.slug, c.name, archived);
        }
    }
    Ok(())
}

/// `channel rename <ws> <slug> <name>`: pre-checks the channel exists (a
/// rename of a nonexistent slug would otherwise be a silent no-op once
/// applied -- `WorkspaceProjection::apply` drops a rename that targets an
/// unknown slug) before sending the control event.
async fn cmd_channel_rename(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    slug: &str,
    name: &str,
) -> anyhow::Result<()> {
    let receipt = ops::channel_rename(engine, client, label, ws, slug, name).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!("renamed #{} to {}", receipt.channel, receipt.name);
    Ok(())
}

/// `channel archive <ws> <slug>`: pre-checks the channel exists (same silent
/// no-op hazard as rename) before sending the control event. Archival is
/// monotonic -- there is no un-archive in M1.
async fn cmd_channel_archive(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    slug: &str,
) -> anyhow::Result<()> {
    let receipt = ops::channel_archive(engine, client, label, ws, slug).await?;
    if output::json_mode() {
        let body = receipt.to_json();
        output::emit_ok(body);
        return Ok(());
    }
    hprintln!("archived #{}", receipt.channel);
    Ok(())
}

#[cfg(test)]
mod tests {
    /// stdin message normalization (M4 ①): a piped body strips exactly one
    /// trailing newline (so `echo "x" | moyu post` sends `x`, not `x\n`) while
    /// preserving message-internal newlines, and an empty/newline-only stdin is
    /// rejected rather than sent as a blank line.
    #[test]
    fn normalize_stdin_message_strips_one_trailing_newline() {
        use super::normalize_stdin_message;
        assert_eq!(normalize_stdin_message("hello\n").as_deref(), Some("hello"));
        assert_eq!(
            normalize_stdin_message("hello\r\n").as_deref(),
            Some("hello")
        );
        assert_eq!(normalize_stdin_message("hello").as_deref(), Some("hello"));
        assert_eq!(
            normalize_stdin_message("line1\nline2\n").as_deref(),
            Some("line1\nline2")
        );
        // Only ONE trailing newline is stripped: a deliberate blank last line
        // survives.
        assert_eq!(
            normalize_stdin_message("hello\n\n").as_deref(),
            Some("hello\n")
        );
        // Nothing meaningful -> None (caller rejects a blank send).
        assert_eq!(normalize_stdin_message(""), None);
        assert_eq!(normalize_stdin_message("\n"), None);
        assert_eq!(normalize_stdin_message("\r\n"), None);
    }

    /// `--import-nsec` piped-stdin path: one line, trailing newline trimmed.
    #[test]
    fn read_nsec_line_reads_and_trims_trailing_newline() {
        use super::read_nsec_line;
        let cursor = std::io::Cursor::new(b"nsec1abc\n".to_vec());
        let got = read_nsec_line(cursor).unwrap();
        assert_eq!(&*got, "nsec1abc");
    }

    #[test]
    fn read_nsec_line_trims_trailing_cr() {
        use super::read_nsec_line;
        let cursor = std::io::Cursor::new(b"nsec1abc\r\n".to_vec());
        let got = read_nsec_line(cursor).unwrap();
        assert_eq!(&*got, "nsec1abc");
    }

    /// An empty (or newline-only) piped line is a caller error, not a
    /// silently-imported empty secret.
    #[test]
    fn read_nsec_line_errors_on_empty_line() {
        use super::read_nsec_line;
        let cursor = std::io::Cursor::new(b"\n".to_vec());
        assert!(read_nsec_line(cursor).is_err());
    }

    /// A last line with no newline at all (`printf %s "$NSEC" | moyu init
    /// --import-nsec`) is read whole.
    #[test]
    fn read_nsec_line_reads_a_line_without_a_newline() {
        use super::read_nsec_line;
        let cursor = std::io::Cursor::new(b"nsec1abc".to_vec());
        assert_eq!(&*read_nsec_line(cursor).unwrap(), "nsec1abc");
    }

    /// Stray spaces/tabs around the key (a copy-paste artefact, `echo " $K"`)
    /// are not part of it; only the first line is taken.
    #[test]
    fn read_nsec_line_trims_surrounding_whitespace() {
        use super::read_nsec_line;
        let cursor = std::io::Cursor::new(b" \tnsec1abc \t\r\nsecond line\n".to_vec());
        assert_eq!(&*read_nsec_line(cursor).unwrap(), "nsec1abc");
        let blank = std::io::Cursor::new(b"  \t \n".to_vec());
        assert!(read_nsec_line(blank).is_err(), "whitespace only is no key");
    }

    /// Bytes that are not UTF-8 are an error, not a truncated or lossy key.
    #[test]
    fn read_nsec_line_rejects_non_utf8() {
        use super::read_nsec_line;
        let cursor = std::io::Cursor::new(vec![b'n', b's', 0xff, 0xfe, b'\n']);
        assert!(read_nsec_line(cursor).is_err());
    }

    #[test]
    fn relays_new_to_lists_only_relays_the_code_adds() {
        let known = vec!["wss://a".to_owned(), "wss://b".to_owned()];
        let offered = vec![
            "wss://b".to_owned(),
            "wss://c".to_owned(),
            "wss://c".to_owned(),
            "wss://d".to_owned(),
        ];
        assert_eq!(
            super::relays_new_to(&known, &offered),
            ["wss://c", "wss://d"]
        );
        assert!(super::relays_new_to(&known, &known).is_empty());
    }

    #[test]
    fn trim_in_place_does_not_reallocate() {
        let mut s = String::with_capacity(64);
        s.push_str("  \tkey material\r\n");
        let (ptr, cap) = (s.as_ptr(), s.capacity());
        super::trim_in_place(&mut s);
        assert_eq!(s, "key material");
        assert_eq!((s.as_ptr(), s.capacity()), (ptr, cap), "same buffer");
    }

    #[test]
    fn read_nsec_line_errors_on_no_input() {
        use super::read_nsec_line;
        let cursor = std::io::Cursor::new(Vec::new());
        assert!(read_nsec_line(cursor).is_err());
    }

    /// `resolve_import_nsec`'s two testable branches (no real stdin/tty
    /// involved): the flag absent stays `None`, and an inline literal (not
    /// the `-` `default_missing_value` sentinel) passes straight through.
    #[test]
    fn resolve_import_nsec_none_stays_none() {
        use super::resolve_import_nsec;
        assert!(resolve_import_nsec(None).unwrap().is_none());
    }

    #[test]
    fn resolve_import_nsec_literal_passes_through() {
        use super::resolve_import_nsec;
        let got = resolve_import_nsec(Some("nsec1literal".to_owned()))
            .unwrap()
            .unwrap();
        assert_eq!(&*got, "nsec1literal");
    }
}
