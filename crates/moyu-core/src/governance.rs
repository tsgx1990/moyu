//! moyu M2 workspace governance — pure decision helpers.
//!
//! A moyu workspace is one MLS group, and MDK enforces a **binary** admin
//! model: membership-changing ops (invite / remove / rename-group /
//! admin-policy) require the caller be in the group's admin set, engine-side
//! (`require_admin`, `cgka-engine/src/app_components.rs:187`). moyu's own
//! control-plane ops (channel create/rename, workspace rename) ride as kind-1210
//! app messages and are NOT admin-gated.
//!
//! Everything here is pure (no I/O, no MDK client) so each rule is unit-tested.
//! The async plumbing (`AppClient::remove_members` / `promote_admin` /
//! `demote_admin` / `self_demote_admin` / `leave_group`) lives in moyu-cli,
//! which feeds these helpers the already-read admin set + roster.
//!
//! ## Why governance *notices* are a roster diff, not a `SyncSummary` event
//!
//! An observing member never learns of a peer's kick / promote / demote / join
//! from a sync event. MDK buffers a peer's epoch-advancing commit and applies it
//! only on a later distributed-convergence pass (`retry_group_convergence`),
//! whose return is a `SendSummary { published, message_ids }` — it carries **no**
//! `GroupStateChange`. `SyncSummary.events` likewise never surfaces these over
//! the wire (verified empirically over a live relay; see moyu-cli
//! `drive_convergence`). So the only reliable signal is to **snapshot each
//! group's roster + admin set before a sync/convergence pass and diff it after**
//! — [`RosterSnapshot`] + [`diff_roster`]. A pure roster diff cannot distinguish
//! an admin-initiated removal from a voluntary leave (both just drop the member),
//! so the two collapse to [`GovChange::Departed`].
//!
//! ## The admin-set join key (verified against MDK rev `4c611ef`)
//!
//! `AppGroupRecord.admin_policy.admins: Vec<String>` are `hex::encode` of each
//! admin's 32-byte account pubkey (`AppGroupAdminPolicyComponent::new`,
//! `marmot-app/src/groups.rs:536` — `admins.iter().map(hex::encode)`). An
//! `AppGroupMemberRecord.member_id_hex` is `hex::encode(member.id.as_slice())`
//! (`client/mod.rs:308`), and a member's id bytes ARE its account pubkey
//! (`admin_pubkey_from_member_id` just returns `member_id.as_slice()` as
//! `[u8;32]`, `marmot-app/src/ids.rs:29`). Both are therefore lowercase
//! `hex::encode` of identical 32 bytes, so **a member is an admin iff
//! `admins` contains its `member_id_hex`**.
//!
//! Do **not** use `AppGroupMemberRecord.account` for this: it is
//! `profiles_by_id().get(member_id_hex)`, which resolves only the LOCAL
//! account(s) — it is `None` for every remote member (`members_with_profiles`,
//! `client/mod.rs:299-317`). `.local` is likewise "is this one of my own
//! accounts", not "is admin".

use std::collections::BTreeSet;

/// True iff `member_id_hex` is in the group's admin set. Both sides are
/// lowercase `hex::encode` of a 32-byte pubkey (see module docs), but we
/// lowercase both defensively so a caller passing upper-case hex still matches.
pub fn is_admin(member_id_hex: &str, admins: &[String]) -> bool {
    let needle = member_id_hex.to_ascii_lowercase();
    admins.iter().any(|a| a.to_ascii_lowercase() == needle)
}

/// A roster row: a member id plus whether it is a group admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRole {
    pub member_id_hex: String,
    pub is_admin: bool,
}

/// Join member ids against the admin set into roster rows, ordered **admins
/// first, then by id** so a rendered roster is stable and admins group
/// visually. Operates on plain hex strings (not the MDK `AppGroupMemberRecord`)
/// to stay decoupled and trivially testable; moyu-cli extracts the
/// `member_id_hex`es from `AppClient::members` and the `admins` from
/// `engine.groups()[…].admin_policy.admins`.
pub fn member_roles(member_id_hexes: &[String], admins: &[String]) -> Vec<MemberRole> {
    let mut rows: Vec<MemberRole> = member_id_hexes
        .iter()
        .map(|h| MemberRole {
            member_id_hex: h.clone(),
            is_admin: is_admin(h, admins),
        })
        .collect();
    // Admins first (`true` > `false` so reverse-compare the flag), then by id.
    rows.sort_by(|a, b| {
        b.is_admin
            .cmp(&a.is_admin)
            .then_with(|| a.member_id_hex.cmp(&b.member_id_hex))
    });
    rows
}

/// What the local member must do before it can `leave_group`, given two
/// engine-enforced MDK rules:
/// 1. an admin CANNOT self-remove while still in the admin set — `leave_group`
///    fails `AdminCannotSelfRemove` **even when co-admins exist**
///    (`send.rs:642`);
/// 2. the admin set cannot be emptied — `self_demote_admin` down to zero fails
///    the empty-list guard (`decode_admin_policy`, `app_components.rs:644`).
///
/// Pure decision from the local member's id + the current admin set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeavePlan {
    /// Not an admin — `leave_group` directly.
    LeaveNow,
    /// An admin, and at least one OTHER admin already exists — `self_demote_admin`
    /// then `leave_group` (demoting self won't empty the set).
    SelfDemoteThenLeave,
    /// The SOLE admin — must first `promote_admin(target)`, then
    /// `self_demote_admin`, then `leave_group`. `leave_group` alone hits rule 1;
    /// `self_demote_admin` alone hits rule 2. The CLI must be given (or must
    /// ask for) a `--transfer-to` target for this case.
    TransferThenLeave,
}

/// Decide the [`LeavePlan`] for the local member. `self_member_id_hex` is the
/// local member's `member_id_hex` (find it via the `AppGroupMemberRecord` whose
/// `.local == true`); `admins` is the group's current admin set.
pub fn leave_plan(self_member_id_hex: &str, admins: &[String]) -> LeavePlan {
    if !is_admin(self_member_id_hex, admins) {
        return LeavePlan::LeaveNow;
    }
    let me = self_member_id_hex.to_ascii_lowercase();
    let other_admins = admins
        .iter()
        .filter(|a| a.to_ascii_lowercase() != me)
        .count();
    if other_admins >= 1 {
        LeavePlan::SelfDemoteThenLeave
    } else {
        LeavePlan::TransferThenLeave
    }
}

/// A point-in-time snapshot of one group's roster and admin set, as lowercase
/// `member_id_hex`. Cheap value type so a caller can snapshot before a
/// sync/convergence pass and [`diff_roster`] it against the after-state to
/// recover what governance change was applied (see the module docs for why an
/// event stream can't do this). Both sets are lowercased on construction so a
/// caller mixing `members()` and `admin_policy.admins` (already lowercase hex,
/// but defensively normalized) diffs cleanly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RosterSnapshot {
    pub members: BTreeSet<String>,
    pub admins: BTreeSet<String>,
}

impl RosterSnapshot {
    pub fn new(member_id_hexes: &[String], admins: &[String]) -> Self {
        Self {
            members: member_id_hexes
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
            admins: admins.iter().map(|s| s.to_ascii_lowercase()).collect(),
        }
    }
}

/// One membership/admin change observed between two [`RosterSnapshot`]s of the
/// same group. `Departed` covers BOTH an admin-initiated removal and a voluntary
/// leave — a pure roster diff cannot tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GovChange {
    Joined { member_id_hex: String },
    Departed { member_id_hex: String },
    Promoted { member_id_hex: String },
    Demoted { member_id_hex: String },
}

/// Diff two snapshots of the **same** group into governance changes, in a stable
/// order (departures, joins, promotions, demotions; each block sorted by id
/// since the inputs are `BTreeSet`s). Promote/Demote are emitted only for a
/// member present in BOTH snapshots: a member who arrives already-admin yields
/// just `Joined` (its admin status rides the roster badge), and a departed admin
/// yields just `Departed` (no dangling `Demoted`). Pure.
pub fn diff_roster(before: &RosterSnapshot, after: &RosterSnapshot) -> Vec<GovChange> {
    let mut out = Vec::new();
    for m in before.members.difference(&after.members) {
        out.push(GovChange::Departed {
            member_id_hex: m.clone(),
        });
    }
    for m in after.members.difference(&before.members) {
        out.push(GovChange::Joined {
            member_id_hex: m.clone(),
        });
    }
    let in_both = |m: &String| before.members.contains(m) && after.members.contains(m);
    for m in after.admins.difference(&before.admins) {
        if in_both(m) {
            out.push(GovChange::Promoted {
                member_id_hex: m.clone(),
            });
        }
    }
    for m in before.admins.difference(&after.admins) {
        if in_both(m) {
            out.push(GovChange::Demoted {
                member_id_hex: m.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A distinct 64-char lowercase hex id (32 bytes) for each test actor.
    fn id(byte: u8) -> String {
        hex::encode([byte; 32])
    }

    #[test]
    fn is_admin_matches_by_member_id_hex_case_insensitively() {
        let admins = vec![id(0xaa), id(0xbb)];
        assert!(is_admin(&id(0xaa), &admins));
        assert!(is_admin(&id(0xaa).to_uppercase(), &admins)); // defensive lowercase
        assert!(!is_admin(&id(0xcc), &admins));
        assert!(!is_admin(&id(0xaa), &[])); // empty admin set → nobody is admin
    }

    #[test]
    fn member_roles_orders_admins_first_then_by_id() {
        // members in arbitrary order; only aa and cc are admins.
        let members = vec![id(0xbb), id(0xcc), id(0xaa), id(0xdd)];
        let admins = vec![id(0xcc), id(0xaa)];
        let rows = member_roles(&members, &admins);
        // Admins first, each block sorted by id.
        assert_eq!(
            rows,
            vec![
                MemberRole {
                    member_id_hex: id(0xaa),
                    is_admin: true
                },
                MemberRole {
                    member_id_hex: id(0xcc),
                    is_admin: true
                },
                MemberRole {
                    member_id_hex: id(0xbb),
                    is_admin: false
                },
                MemberRole {
                    member_id_hex: id(0xdd),
                    is_admin: false
                },
            ]
        );
    }

    #[test]
    fn leave_plan_non_admin_leaves_now() {
        let admins = vec![id(0xaa)];
        assert_eq!(leave_plan(&id(0xbb), &admins), LeavePlan::LeaveNow);
    }

    #[test]
    fn leave_plan_admin_with_co_admin_self_demotes() {
        // Two admins incl. me → a co-admin remains after I self-demote.
        let admins = vec![id(0xaa), id(0xbb)];
        assert_eq!(
            leave_plan(&id(0xaa), &admins),
            LeavePlan::SelfDemoteThenLeave
        );
    }

    #[test]
    fn leave_plan_sole_admin_must_transfer_first() {
        // I am the only admin → must promote someone before I can self-demote.
        let admins = vec![id(0xaa)];
        assert_eq!(leave_plan(&id(0xaa), &admins), LeavePlan::TransferThenLeave);
    }

    #[test]
    fn diff_roster_reports_join_depart_promote_demote() {
        // before: aa (admin), bb, cc(admin)   after: aa (admin), bb (admin), dd
        //   cc departed, dd joined, bb promoted, and aa unchanged.
        let before = RosterSnapshot::new(&[id(0xaa), id(0xbb), id(0xcc)], &[id(0xaa), id(0xcc)]);
        let after = RosterSnapshot::new(&[id(0xaa), id(0xbb), id(0xdd)], &[id(0xaa), id(0xbb)]);
        let changes = diff_roster(&before, &after);
        assert_eq!(
            changes,
            vec![
                GovChange::Departed {
                    member_id_hex: id(0xcc)
                },
                GovChange::Joined {
                    member_id_hex: id(0xdd)
                },
                GovChange::Promoted {
                    member_id_hex: id(0xbb)
                },
            ]
        );
    }

    #[test]
    fn diff_roster_no_change_is_empty() {
        let snap = RosterSnapshot::new(&[id(0xaa), id(0xbb)], &[id(0xaa)]);
        assert!(diff_roster(&snap, &snap).is_empty());
    }

    #[test]
    fn diff_roster_join_as_admin_yields_only_joined() {
        // dd both joins AND is already in the admin set of `after` -> a single
        // Joined, never a spurious Promoted for a member absent from `before`.
        let before = RosterSnapshot::new(&[id(0xaa)], &[id(0xaa)]);
        let after = RosterSnapshot::new(&[id(0xaa), id(0xdd)], &[id(0xaa), id(0xdd)]);
        assert_eq!(
            diff_roster(&before, &after),
            vec![GovChange::Joined {
                member_id_hex: id(0xdd)
            }]
        );
    }

    #[test]
    fn diff_roster_departed_admin_yields_only_departed() {
        // cc was an admin and leaves -> a single Departed, no dangling Demoted.
        let before = RosterSnapshot::new(&[id(0xaa), id(0xcc)], &[id(0xaa), id(0xcc)]);
        let after = RosterSnapshot::new(&[id(0xaa)], &[id(0xaa)]);
        assert_eq!(
            diff_roster(&before, &after),
            vec![GovChange::Departed {
                member_id_hex: id(0xcc)
            }]
        );
    }

    #[test]
    fn diff_roster_demote_of_staying_member() {
        // bb stays a member but loses admin -> Demoted.
        let before = RosterSnapshot::new(&[id(0xaa), id(0xbb)], &[id(0xaa), id(0xbb)]);
        let after = RosterSnapshot::new(&[id(0xaa), id(0xbb)], &[id(0xaa)]);
        assert_eq!(
            diff_roster(&before, &after),
            vec![GovChange::Demoted {
                member_id_hex: id(0xbb)
            }]
        );
    }

    #[test]
    fn roster_snapshot_lowercases_for_case_insensitive_diff() {
        let before = RosterSnapshot::new(&[id(0xaa).to_uppercase()], &[id(0xaa).to_uppercase()]);
        let after = RosterSnapshot::new(&[id(0xaa)], &[id(0xaa)]);
        assert!(diff_roster(&before, &after).is_empty());
    }
}
