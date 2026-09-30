# MDK API map for moyu's M0 loop

Every row below was confirmed by reading MDK source directly (the MDK source at
the pinned revision, workspace `version = "0.9.2"`, all crates MIT /
`publish = false`), not from MDK's docs or a secondary summary of them.
File:line citations are as precise as the research passes could make them;
a few (noted inline) are "somewhere in this ~100-line span" rather than an
exact single line, because the function body was read but its start line
wasn't independently re-confirmed by a second pass. **This was written
without a Rust toolchain available to compile against — treat every entry
as "very likely correct, re-check against `cargo doc`/rust-analyzer on first
build," not as gospel.** See `README.md` for the consolidated uncertain-point
list.

## 0. Package name corrections (directory name ≠ crate name)

| Directory | Real `[package] name` | Import as |
|---|---|---|
| `crates/traits` | `cgka-traits` | `cgka_traits::...` |
| `crates/cli` | `wn-cli` (bins `wn`, `wnd`) | n/a (not a moyu dependency) |
| everything else | matches its directory name | — |

The task brief that kicked off this build said the traits crate is named
`traits`, not `cgka-traits` — reading `crates/traits/Cargo.toml` directly
shows `name = "cgka-traits"` with a comment confirming this is deliberate
("Workspace dep key is `cgka-traits`; code uses `cgka_traits::...`"). moyu's
`Cargo.toml` uses `cgka-traits`, trusting the file over the brief.

## 1. Architecture decision: skip the daemon

MDK's own CLI (`wn`) can run standalone (one `MarmotApp` per process) or
against a long-lived `wnd` daemon (`MarmotAppRuntime`/`AccountManager`, one
background worker per account, a `broadcast::Sender<MarmotAppEvent>` event
bus). **Both paths call the exact same underlying functions** —
`crates/cli/src/daemon/runtime_host.rs`'s dispatch table
(`dispatch_hosted_runtime_command`, ~line 151+) calls the identical
`commands::*_command_with_runtime` functions the no-daemon path calls
directly (`crates/cli/src/lib.rs:402-558`, `run_cli_local`/`execute_inner`).
The daemon/actor machinery exists purely to support live multi-subscriber
event delivery and multi-process access — neither of which a single-process
CLI needs.

**moyu uses `MarmotApp` + `AppClient` directly, no `MarmotAppRuntime`/daemon
actor**, except for the one call (`create_or_import_account`) that only
exists on `MarmotAppRuntime`. See `crates/moyu-core/src/engine.rs`.

## 2. Step-by-step: moyu M0 loop → exact MDK calls

### Step 1+2: identity + first KeyPackage (kind 30443)

| What | Call | Location |
|---|---|---|
| Construct the app | `MarmotApp::with_relays_and_account_home(root: impl AsRef<Path>, relay_urls: Vec<String>, account_home: AccountHome) -> Self` | `crates/marmot-app/src/lib.rs:951-962` |
| Open account home with a custom secret store | `AccountHome::open_with_secret_store(root: impl AsRef<Path>, secret_store: Arc<dyn AccountSecretStore>) -> Self` | `crates/marmot-account/src/home.rs:118-127` |
| Get a runtime handle | `MarmotApp::runtime(&self) -> MarmotAppRuntime` (cheap, stateless) | `crates/marmot-app/src/lib.rs:994-996` |
| Create-or-import + publish relay lists + first KeyPackage, one call | `MarmotAppRuntime::create_or_import_account(&self, request: AccountSetupRequest) -> Result<AccountSetupResult, AppError>` | `crates/marmot-app/src/runtime/mod.rs:2421-2426` (shared impl body: `runtime/mod.rs:2902-2993`) |
| Request shape | `AccountSetupRequest { identity: Option<String>, default_relays: Vec<TransportEndpoint>, bootstrap_relays: Vec<TransportEndpoint>, publish_missing_relay_lists: bool, publish_initial_key_package: bool }` — `identity: None` = brand-new keypair, `Some(nsec)` = import, `Some(npub)` = watch-only (no local secret) | `crates/marmot-app/src/runtime/mod.rs:675-689` |
| Result shape | `AccountSetupResult { account: AccountSummary, relay_lists: AccountRelayListStatus, key_package_bytes: Option<usize>, profile: Option<UserProfileMetadata> }` | same file, same lines |
| Underlying identity creation (what the above calls into) | `AccountHome::create_nostr_account(&self) -> AccountHomeResult<AccountSummary>` (fresh `nostr::Keys::generate()`, label = pubkey hex) / `import_nostr_account(&self, secret_key: &str)` | `crates/marmot-account/src/home.rs:142-161` |
| `AccountSummary` shape | `{ label: String, account_id_hex: String, local_signing: bool, signed_out: bool }` | `crates/marmot-account/src/home.rs:69-80` |
| All-or-nothing rollback if setup fails partway | `rollback_account_after_setup_failure` | `crates/marmot-app/src/runtime/mod.rs:3145-3161` |
| Publish/re-publish a KeyPackage on a live client (used by moyu directly instead of going through the runtime again) | `AppClient::publish_key_package(&mut self) -> Result<KeyPackage, AppError>` | `crates/marmot-app/src/client/mod.rs:190-208` |
| Force-rotate (mint brand-new regardless of cache) | `AppClient::rotate_key_package(&mut self) -> Result<KeyPackage, AppError>` | `crates/marmot-app/src/client/mod.rs:210-217` |
| KeyPackage event kind, confirmed as a real constant | `pub const KIND_MARMOT_KEY_PACKAGE: u64 = 30_443;` | `crates/transport-nostr-adapter/src/key_package.rs:11` (test assertion at `key_package.rs:191`; also asserted `sdk_client.rs:1000`) |
| `d` tag (addressable-event slot id) | opaque, caller-supplied `key_package_slot_id: String` field, **not derived from anything** inside the adapter | `crates/transport-nostr-adapter/src/key_package.rs:25,41-45,79` |
| KeyPackage-level metadata (ref hash + credential identity — **no timestamp field**, why moyu tracks publish time itself) | `pub struct KeyPackageMetadata { pub key_package_ref_hex: String, pub credential_identity_hex: String }`; `pub fn key_package_metadata(kp: &cgka_traits::engine::KeyPackage) -> Result<KeyPackageMetadata, cgka_traits::error::EngineError>` | `crates/cgka-engine/src/key_package.rs:23-33` — **read directly, not via a research pass; highest-confidence entry in this doc** |

### Step 3: create a 1:1 group ("founding creation with initial invitees")

| What | Call | Location |
|---|---|---|
| One call does everything | `AppClient::create_group(&mut self, name: &str, member_refs: &[&str]) -> Result<GroupId, AppError>` | `crates/marmot-app/src/client/mod.rs:219-277` |
| `member_refs` accepts | npub bech32 **or** hex pubkey, directly — no need to pre-fetch a `KeyPackage` yourself | resolved internally via `MarmotApp::member_key_package`, `crates/marmot-app/src/lib.rs:2266-2312` (resolution order: local account cache → directory cache → live relay fetch of kind-30443 events, freshest wins) |
| Underlying engine call (for reference; moyu never calls this layer directly) | `CgkaEngine::create_group(&mut self, req: CreateGroupRequest) -> Result<(GroupId, SendResult), EngineError>`, impl `Engine::do_create_group` | trait: `crates/traits/src/engine.rs:639`; impl: `crates/cgka-engine/src/group_lifecycle.rs:57` |
| Why "founding" needs no separate commit | `SendResult::GroupCreated { welcomes: Vec<TransportMessage>, pending: PendingStateRef }` — the founding commit is dropped; every initial member (other than the creator) joins purely via `welcomes` | rationale documented at `crates/cgka-engine/src/group_lifecycle.rs:306-317`; type at `crates/traits/src/engine.rs:171-174` |
| Welcome publish is automatic, not a separate step | `AccountDeviceRuntime::create_group_with_audit_context` immediately drains `PublishWork::GroupCreated` and gift-wraps + publishes every Welcome inside the same call | `crates/marmot-account/src/runtime.rs:255-267, 470-521, 638-793` |
| Adding a member to an *existing* group (not needed for 1:1, noted for M1 small-group work) | `AppClient::invite_members(&mut self, group_id: &GroupId, member_refs: &[&str]) -> Result<SendSummary, AppError>` | `crates/marmot-app/src/client/mod.rs:451-458` (full body through ~581) |
| Invitee-side: clearing the "pending confirmation" flag after a Welcome auto-joined MLS state | `AppClient::accept_group_invite(&mut self, group_id: &GroupId) -> Result<AppGroupRecord, AppError>` — reported as **synchronous, no `.await`** in the call site; **TODO(verify)** this against the actual `fn`/`async fn` keyword on first build | `crates/marmot-app/src/client/mod.rs:785-787`, `set_group_invite_confirmation` at `:1804-1822` |

### Step 4: send / sync / receive application messages

| What | Call | Location |
|---|---|---|
| Send a chat message | `AppClient::send(&mut self, group_id: &GroupId, payload: &[u8]) -> Result<SendSummary, AppError>` (payload must be valid UTF-8) | `crates/marmot-app/src/client/mod.rs:1023-1030` |
| Send result shape | `SendSummary { published: usize, message_ids: Vec<String> }` | `crates/marmot-app/src/lib.rs:661-664` |
| Bounded one-shot sync (drain everything currently available, return) | `AppClient::sync(&mut self) -> Result<SyncSummary, AppError>` | `crates/marmot-app/src/client/sync.rs:55-71` |
| Blocking long-poll ("wait for the next real event") — **the primitive moyu's REPL receive loop is built on** | `AppClient::next_event(&mut self) -> Result<SyncSummary, AppError>` — loops on `self.adapter.receive().await?`, skips echoes/dupes, returns once something new landed | `crates/marmot-app/src/client/sync.rs:170-212` |
| Sync result shape | `SyncSummary { joined_groups: Vec<GroupId>, messages: Vec<ReceivedMessage>, events: Vec<GroupEvent>, projection_updates: Vec<AppProjectionUpdate> }` | `crates/marmot-app/src/lib.rs:560-578` |
| Decrypted incoming message shape | `ReceivedMessage { message_id_hex, source_message_id_hex, sender: String (hex, MLS-authenticated), sender_display_name: Option<String>, group_id: GroupId, source_epoch: u64, plaintext: String, kind: u64, tags: Vec<Vec<String>>, recorded_at: u64 }` | `crates/marmot-app/src/lib.rs:580-599` |
| Underlying engine send (application message) | `Engine::do_send_app_message` → `SendResult::ApplicationMessage { msg: TransportMessage }`; calls `mls_group.create_message(..)` then `peeler.wrap_group_message_with_metadata(..)` | `crates/cgka-engine/src/message_processor/send.rs:741` |
| Underlying engine ingest (decrypt) | `CgkaEngine::ingest(&mut self, msg: TransportMessage) -> Result<IngestOutcome, EngineError>` → `Engine::ingest_group_message`; decrypted payload surfaces via `drain_events()` as `GroupEvent::MessageReceived { group_id, sender, epoch, payload }` | trait: `crates/traits/src/engine.rs:537`; impl: `crates/cgka-engine/src/message_processor/ingest.rs:85`; event: `crates/traits/src/engine.rs:344-349` |
| Group message event kind, confirmed | `pub const KIND_MARMOT_GROUP_MESSAGE: u64 = 445;` (proposals/commits/application messages all unified under this one kind — relay cannot distinguish them) | `crates/transport-nostr-peeler/src/lib.rs:24` |
| Ephemeral signing per kind-445 event (never the real identity) | `let ephemeral = Keys::generate();` per send, signed with it; adapter **hard-rejects** publishing an unsigned/identity-signed 445 event | `crates/transport-nostr-peeler/src/peeler.rs:139-174`; enforcement + test: `crates/transport-nostr-adapter/src/sdk_client.rs:245-268, 948-975` |

## 3. Nostr event-kind reference (all confirmed as real constants in code, not assumed from spec prose)

| Purpose | Kind | Constant | Defined at |
|---|---|---|---|
| KeyPackage (addressable) | 30443 | `KIND_MARMOT_KEY_PACKAGE` | `transport-nostr-adapter/src/key_package.rs:11` |
| Group message (proposal/commit/app, unified) | 445 | `KIND_MARMOT_GROUP_MESSAGE` | `transport-nostr-peeler/src/lib.rs:24` |
| Welcome outer (NIP-59 gift wrap) | 1059 | `KIND_NIP59_GIFT_WRAP` | `transport-nostr-peeler/src/lib.rs:27` |
| Welcome rumor (inner, unsigned) | 444 | `KIND_MARMOT_WELCOME_RUMOR` | `transport-nostr-peeler/src/lib.rs:30` |
| NIP-65 relay list | 10002 | `KIND_NIP65_RELAY_LIST` | `transport-nostr-adapter/src/relay_list.rs:4` |
| Marmot inbox relay list | 10050 | `KIND_MARMOT_INBOX_RELAY_LIST` | `transport-nostr-adapter/src/relay_list.rs:5` |
| NIP-59 seal | 13 (per NIP-59 spec) | *no local MDK constant* | inside the `nostr` crate's own `nip59` module — opaque to MDK, used via `EventBuilder::gift_wrap(..)` |

These numbers match the event-kind table in the spec exactly; this pass just
re-confirmed them against code instead of trusting a spec-only reading.

Welcome rumor also carries required tags: `e` = hex(KeyPackage event id it
consumed), `relays` = 1-16 relay URLs the new member should use next
(`crates/transport-nostr-peeler/src/peeler.rs:357-364, 427-449`).

## 4. Transport & storage wiring is automatic — moyu does not touch it

Constructing `MarmotApp::with_relays_and_account_home(root, relay_urls,
account_home)` wires up, entirely internally (via `marmot-app`'s own
`relay_plane` module):

- a `transport_nostr_adapter::NostrTransportAdapter` (implements the shared
  `cgka_traits::transport_adapter::TransportAdapter` trait,
  `crates/traits/src/transport_adapter.rs:289`) backed by
  `NostrSdkRelayClient` (needs the adapter's `sdk` cargo feature — enabled
  workspace-wide in moyu's root `Cargo.toml`; this is the **only** production
  `NostrRelayClient` implementation in the crate, confirmed at
  `crates/transport-nostr-adapter/src/sdk_client.rs:73-88` and required by
  `marmot-app` itself: `crates/marmot-app/Cargo.toml:43`);
- a `transport_nostr_peeler::NostrMlsPeeler` (implements
  `cgka_traits::peeler::TransportPeeler`,
  `crates/traits/src/peeler.rs:115-154`);
- one `storage_sqlite::SqliteAccountStorage` (implements
  `cgka_traits::storage::StorageProvider`,
  `crates/traits/src/storage.rs:241`, impl at
  `crates/storage-sqlite/src/connection.rs:626`) per account, auto-migrated
  to the current schema (25 migrations,
  `crates/storage-sqlite/src/migrations.rs`) on open.

**SQLCipher key derivation for every one of those databases is fully
automatic and already handled** by `marmot-app`'s own (crate-private)
`src/sqlcipher.rs`: HKDF-SHA256 keyed off the account's actual Nostr secret
key plus a persisted-once-per-database random 32-byte salt sidecar file,
domain-separated per logical database (`SqlcipherDatabaseKind`: session /
account-projection / directory-cache).
`derive_sqlcipher_key_material` (`crates/marmot-app/src/sqlcipher.rs:253-273`)
is the exact function; it is `pub(crate)`-only, so moyu cannot call it, but
also **does not need to** — every database `MarmotApp`/`AppClient` opens
already gets this for free. An earlier design
assumed that moyu would need to write this derivation itself from
scratch (that assumption was based on `storage-sqlite` alone, which indeed
takes only a bare `SqlCipherKey` with no derivation logic — true in
isolation, but moot once moyu depends on `marmot-app` instead of
`storage-sqlite` directly).

`storage-sqlite::SqliteAccountStorage`'s own encrypted-open constructor, for
reference (not called by moyu directly):
```rust
pub fn open_encrypted(path: impl AsRef<Path>, key: &SqlCipherKey) -> StorageResult<Self>
```
`crates/storage-sqlite/src/connection.rs:427`. `SqlCipherKey` (`:324-337`) is
`SqlCipherKey(Zeroizing<String>)`, constructed via `SqlCipherKey::new(impl
Into<String>)` — so it takes a passphrase-shaped string, not raw bytes,
zeroized on drop.

**What this leaves for moyu to actually encrypt**: the Nostr **secret key**
(`nsec`) itself at rest — MDK's default `AccountSecretStore` impl
(`LocalFileSecretStore`) writes it as plaintext JSON, `0600`-protected only
(`crates/marmot-account/src/secret_store.rs:74-125`). See §5.

## 5. The `AccountSecretStore` plug-in point (moyu's Argon2id integration)

```rust
pub trait AccountSecretStore: Send + Sync {
    fn has_secret_for_label(&self, label: &str) -> AccountHomeResult<bool>;
    fn has_secret_for_account_id(&self, _account_id_hex: &str) -> AccountHomeResult<bool> { Ok(false) }
    fn write_secret(&self, account: &AccountSummary, keys: &nostr::Keys) -> AccountHomeResult<()>;
    fn load_secret(&self, account: &AccountSummary) -> AccountHomeResult<nostr::Keys>;
    fn remove_secret(&self, account: &AccountSummary) -> AccountHomeResult<()>;
}
```
`crates/marmot-account/src/secret_store.rs:60-71`. Two built-in impls exist
(`LocalFileSecretStore` — plaintext; `KeychainSecretStore` — OS keyring,
`secret_store.rs:74-241`, `keyring.rs`), but `AccountHome::open_with_secret_store`
(`home.rs:118-127`) accepts **any** implementation. `moyu-core`'s
`identity::Argon2idFileSecretStore` (`crates/moyu-core/src/identity.rs`) is a
from-scratch impl (not a fork of either built-in) doing Argon2id(passphrase,
random salt) → XChaCha20-Poly1305, matching the original
design intent. The passphrase itself is held in memory for the store's lifetime
(the trait's methods take no passphrase parameter), supplied once at
construction from a CLI prompt.

`AccountHomeError` variants used by moyu's impl (all confirmed by reading
`crates/marmot-account/src/error.rs:14-52` directly): `Io(#[from]
std::io::Error)`, `Json(#[from] serde_json::Error)`, `Hex(#[from]
hex::FromHexError)`, `SecretStore(String)`, `SecretNotFound(String)`,
`InvalidSecretKey`.

## 6. `GroupId` / `TransportEndpoint` (confirmed directly)

```rust
// crates/traits/src/types.rs:11-55 (byte_id! macro)
pub struct GroupId(Vec<u8>);
impl GroupId {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self;
    pub fn as_slice(&self) -> &[u8];
    pub fn into_bytes(self) -> Vec<u8>;
}
impl fmt::Display for GroupId { /* hex::encode */ }
```
No dedicated `from_hex`/`to_hex` — round-trip via `hex::decode(s)` +
`GroupId::new(bytes)` / `group_id.to_string()`.

```rust
// crates/traits/src/transport_adapter.rs:22-48
pub struct TransportEndpoint(pub String);
impl From<&str> for TransportEndpoint { .. }
impl From<String> for TransportEndpoint { .. }
```

## 7. Left for moyu-cli/M1 to wire (not needed for the M0 happy path)

- A plain `AccountHome::account(&self, label: &str) -> AccountHomeResult<AccountSummary>`
  passthrough (confirmed to exist, `home.rs:190-205`) isn't exposed through
  `moyu_core::engine::MoyuEngine` yet — `moyu whoami` currently prints the
  locally-persisted active label + derives its npub directly (valid because
  `create_nostr_account` sets label = pubkey hex), rather than round-tripping
  through `AccountHome` for a value already known locally. A fuller `whoami`
  (relay-list status, signed-out state) would want this added.
- `MarmotApp::fetch_latest_key_package_for_account_id` /
  `AccountRelayListStatus` inspection (confirmed at
  `crates/marmot-app/src/directory/methods.rs:224-228` /
  `crates/marmot-app/src/lib.rs:708-717`) aren't wrapped — `create_group`
  already does KeyPackage resolution internally, so moyu-core never needs to
  call these directly for the M0 flow.
- `AppClient::invite_members` (multi-member groups) is confirmed (§2) but
  unused until M1's "small groups" milestone.
- A real KeyPackage-rotation **scheduler** (a timer, not a check-on-demand
  call) is M1 per the design doc; `crate::keypackage_rotation::ensure_fresh`
  is the policy function such a scheduler would call.

## 8. Everything this environment could **not** verify by reading code

Consolidated in `README.md`'s "known-uncertain" list, each tagged
`TODO(verify)` at its exact call site in the source — not repeated here to
avoid the two lists drifting out of sync.

## 9. M2 workspace governance APIs (verified against pinned rev `4c611ef`)

All confirmed by reading the source of the pinned MDK dependency (revision
`4c611ef` of the MDK repository). Everything below is `AppClient`
(`crates/marmot-app/src/client/mod.rs`) unless noted, and admin-gated ops are
enforced **engine-side** (`require_admin`, `cgka-engine/src/app_components.rs:187`)
— a non-admin cannot even construct the commit, and a remote non-admin's commit
is rejected on ingest.

| Op | Signature | Notes |
|---|---|---|
| Kick | `async fn remove_members(&mut self, group_id: &GroupId, member_refs: &[&str]) -> Result<SendSummary, AppError>` (`:583`) | admin-only (`send.rs:349`); real MLS commit / new epoch; can't target self (`send.rs:318`, use Leave); rejects removing every admin (`AdminDepletion`, `send.rs:376`); if target was admin, same commit drops it from admin-policy + emits `AdminRemoved`. No ban-list → re-invite is unrestricted. |
| Promote | `async fn promote_admin(&mut self, group_id, member_ref: &str) -> Result<SendSummary, AppError>` (`:806`) | admin-only; appends to `admin_policy.admins` via `UpdateAppComponents`. |
| Demote other | `async fn demote_admin(&mut self, group_id, member_ref: &str) -> …` (`:826`) | admin-only. |
| Demote self | `async fn self_demote_admin(&mut self, group_id) -> …` (`:845`) | admin-only; **cannot empty** the set — `decode_admin_policy` rejects a zero-length admin list (`app_components.rs:644`). |
| Leave | `async fn leave_group(&mut self, group_id) -> Result<SendSummary, AppError>` (`:621`) | fails `AdminCannotSelfRemove` (`send.rs:642`, err `traits/src/error.rs:36`) if the caller is still in the admin set — **even with co-admins present** (test `admin_cannot_self_remove_even_when_co_admin_present`). Legal creator-leave = `promote_admin(co)` → `self_demote_admin` → `leave_group`. |

**`member_ref` accepted forms** (`MarmotApp::member_id`, `lib.rs:2317`): a **local
account label**, an **npub**, or a **hex pubkey** — it tries
`account_home().account(ref)` first, else `PublicKey::parse(ref)` (which accepts
npub + hex). So the CLI can pass a roster `member_id_hex` straight through.

**Admin-set join key** (the load-bearing fact for roster badges): `admin_policy.
admins` entries and a member's `member_id_hex` are BOTH `hex::encode` of the same
32-byte account pubkey (`groups.rs:536` builds admins via
`admins.iter().map(hex::encode)`; `member_id_hex = hex::encode(member.id)` and
`admin_pubkey_from_member_id` = the id bytes, `ids.rs:29`). → **a member is admin
iff `admin_policy.admins.contains(member_id_hex)`**. Do NOT use
`AppGroupMemberRecord.account` (it's `Some` only for the LOCAL account, `None` for
remote members — `members_with_profiles`, `client/mod.rs:299`); use
`.local == true` only to find *which member is me*.

**Reading roles**: `AppGroupRecord.admin_policy: AppGroupAdminPolicyComponent {
admins: Vec<String>, .. }` (from `engine.groups()`); `members()` →
`Vec<AppGroupMemberRecord { member_id_hex, account: Option<String>, local: bool }>`
carries **no** admin flag. Both types are re-exported from `moyu_core::engine`.

**Governance changes DON'T surface as events for an observer** (verified empirically
over a live relay, increment ③): a peer's kick / promote / demote / join reaches an
observing member only via MDK's **distributed convergence**, applied by
`AppClient::retry_group_convergence(&GroupId) -> Result<SendSummary, AppError>`
(`client/mod.rs:1661`; `SendSummary { published: usize, message_ids: Vec<String> }` —
**no `GroupStateChange`**). `SyncSummary.events`'s `GroupEvent::GroupStateChanged`
variants (`MemberRemoved` / `MemberLeft` / `AdminAdded` / `AdminRemoved` / `MemberAdded`,
each `{ member: MemberId }`) are **never populated over the wire** for the observer
(they fire only on a local same-tick apply, which moyu's actor-less poll loop does not
hit). So moyu derives notices from a **roster+admin snapshot diff** taken before/after
each sync+convergence pass: `moyu_core::governance::{RosterSnapshot, GovChange,
diff_roster}` + moyu-cli `snapshot_rosters` / `governance_diff`. (The earlier
`classify_governance_event` / `GovNotice` on `SyncSummary.events` was removed as proven
dead code.) The same diff also re-arms the §5.2 join fallback (`saw_member_added` on
`summary.events` likewise never fires over the wire).

**No sub-group / private-channel primitive exists** (whole-tree grep: zero hits).
A private channel = a separate MLS group (the deferred 方案 A), OUT of M2.
