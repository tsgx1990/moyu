//! The shared domain layer between `main.rs`'s `cmd_*` command wrappers,
//! `ops.rs`'s print-free command cores, and `session/mod.rs`'s `moyu session`
//! dispatcher: engine construction, workspace/channel resolution, the
//! membership/governance projections, message-kind constants, and the
//! join-request pipeline. Extracted out of `main.rs` (S1 refactor) so the
//! dependency `ops`/`session` -> domain is explicit (`crate::domain::X`)
//! rather than reaching back up into the binary's own root module via
//! `super::X`. `main.rs` itself keeps depending on this module too, via one
//! explicit `use domain::{...};` import block -- it is not special-cased.
//!
//! Submodules are grouped by concern, not by caller:
//! - [`engine`] — engine/session construction (`build_engine`,
//!   `refresh_keypackage`, `reject_loopback_relays`), the wire-send helper
//!   `send_control`, and small id helpers (`active_label`,
//!   `group_id_from_hex`).
//! - [`membership`] — the membership/governance projections: one
//!   `groups()`/`messages()` scan classified into workspaces + private
//!   channels (`membership_view`), roster snapshotting and diffing for
//!   governance notices (`snapshot_rosters`, `governance_diff`), and the
//!   admin-gated command prologue (`workspace_gov`).
//! - [`resolve`] — turning a user-typed `<ws>`/`<channel>` reference (or an
//!   untrusted envelope's exact group id) into a `GroupId`, plus the 1:1 DM
//!   find-or-create.
//! - [`content`] — the Marmot inner-event kind/tag constants (aliased to
//!   `moyu_core::kinds`, the canonical upstream values) and the message/
//!   attachment encode-decode helpers built on them.
//! - [`join`] — the invite/join-request pipeline: pending-request scanning,
//!   the bearer-secret trust check, and `approve_one`.
//!
//! Each submodule's items are `pub(crate)`; this file re-exports the whole
//! surface with `pub(crate) use <submodule>::*;` so every caller writes
//! `crate::domain::X` regardless of which submodule `X` actually lives in.

pub(crate) mod content;
pub(crate) mod engine;
pub(crate) mod join;
pub(crate) mod membership;
pub(crate) mod resolve;

pub(crate) use content::*;
pub(crate) use engine::*;
pub(crate) use join::*;
pub(crate) use membership::*;
pub(crate) use resolve::*;
