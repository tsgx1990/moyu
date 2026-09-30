//! The `moyu chat` REPL loop: bridges blocking terminal input (`rustyline`)
//! with MDK's async send/receive primitives.
//!
//! # Design: single-owner client + `select!` over input and a poll tick
//!
//! `AppClient` is owned by this one task — no `Mutex`, no second task. We
//! multiplex two event sources with `tokio::select!`:
//!   * lines from the blocking `rustyline` reader (delivered over an mpsc from
//!     an OS thread, since rustyline is sync-only), and
//!   * a periodic tick that calls the **bounded** `AppClient::sync()`
//!     (`crates/marmot-app/src/client/sync.rs:55-71`) to drain and print any
//!     newly arrived messages.
//!
//! Both `select!` branch *heads* (`line_rx.recv()`, `interval.tick()`) are
//! cancel-safe; the actual `client.send()` / `client.sync()` calls run to
//! completion *inside* a branch, so their cancel-safety is irrelevant.
//!
//! This deliberately does NOT use `AppClient::next_event()` (sync.rs:170-212):
//! that is an *unbounded* long-poll that only returns once a real inbound event
//! arrives (it even skips our own relay echoes), so driving it from a shared
//! lock — as an earlier version did — makes `next_event()` hold the client for
//! an unbounded (idle = forever) time and permanently starve sends. `sync()`
//! returns promptly whether or not anything arrived, which is exactly what a
//! poll loop needs.

use std::time::{Duration, Instant};

use moyu_core::engine::{AppClient, GroupId};
use moyu_core::store::MoyuStore;

/// How often to poll the relays for newly-arrived inbound messages.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub async fn run_chat_loop(
    mut client: AppClient,
    group_id: GroupId,
    store: &mut MoyuStore,
) -> anyhow::Result<()> {
    // rustyline is sync-only; read lines on a plain OS thread and forward them
    // into the async world over a channel.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let mut editor = match rustyline::DefaultEditor::new() {
            Ok(e) => e,
            Err(e) => {
                heprintln!("[failed to start line editor: {e}]");
                return;
            }
        };
        // Ctrl-D (Eof) / Ctrl-C (Interrupted) / IO error -> stop reading (loop
        // exits when `readline` stops returning `Ok`).
        while let Ok(line) = editor.readline("moyu> ") {
            let _ = editor.add_history_entry(line.as_str());
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Initial catch-up: print anything already waiting for us before the first
    // tick fires.
    if let Err(e) = poll_and_print(&mut client, &group_id).await {
        heprintln!("[sync error: {e}]");
    }

    // Every command already re-checks KeyPackage freshness once at startup
    // (`refresh_keypackage`, called by `open_session`), but a `chat` REPL can
    // be left open for weeks -- re-check on the poll tick, throttled to
    // `KEYPACKAGE_RECHECK` so it stays a cheap local timestamp check the rest
    // of the time.
    let mut last_keypackage_check = Instant::now();

    loop {
        tokio::select! {
            maybe_line = line_rx.recv() => match maybe_line {
                Some(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match client.send(&group_id, line.as_bytes()).await {
                        Ok(summary) => tracing::debug!(message_ids = ?summary.message_ids, "sent"),
                        Err(e) => heprintln!("[send failed: {e}]"),
                    }
                }
                // Input closed (Ctrl-D / editor gone) -> leave the chat.
                None => break,
            },
            _ = poll.tick() => {
                if let Err(e) = poll_and_print(&mut client, &group_id).await {
                    heprintln!("[sync error: {e}]");
                }
                let now = Instant::now();
                if crate::domain::keypackage_recheck_due(last_keypackage_check, now) {
                    last_keypackage_check = now;
                    crate::domain::refresh_keypackage(&mut client, store).await;
                }
            }
        }
    }

    hprintln!("Input closed, leaving chat.");
    Ok(())
}

/// Drain currently-available inbound events via the bounded `sync()` and print
/// any that belong to `group_id`. Our own outbound messages are filtered out by
/// MDK's relay-echo suppression inside `sync()`, so they are not re-printed.
async fn poll_and_print(client: &mut AppClient, group_id: &GroupId) -> anyhow::Result<()> {
    let summary = client.sync().await?;
    for msg in summary.messages {
        if msg.group_id == *group_id {
            let who = msg.sender_display_name.unwrap_or(msg.sender);
            hprintln!(
                "{who}: {}",
                crate::output::indent_continuation(&msg.plaintext)
            );
        }
    }
    // Apply any peer handshake commit for this group that `sync()` only buffered
    // into MDK's convergence subsystem (MDK applies peer commits on a later
    // convergence pass, not inline -- see the CLI's `drive_convergence`). A 1:1
    // DM rarely produces governance commits, but keeping the receive path
    // correct is cheap: `retry_group_convergence` is a no-op when nothing is
    // buffered.
    let _ = client.retry_group_convergence(group_id).await;
    Ok(())
}
