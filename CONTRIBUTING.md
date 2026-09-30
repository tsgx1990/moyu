# Contributing

moyu is in maintenance mode: security fixes and MDK upgrades are welcome,
new features generally are not (upstream MDK's own `wn` client is where new
protocol work lands). Open an issue before writing anything large.

**Security problems:** do not open a public issue or pull request. Use the
private reporting form described in [`SECURITY.md`](SECURITY.md).

## Building and testing

Rust 1.97.1 is pinned by `rust-toolchain.toml`. The four gates CI enforces:

```sh
cargo fmt --all --check
cargo build --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo test -p moyu-core -p moyu-cli
```

The desktop shell (`apps/desktop`) has its own: `pnpm typecheck`,
`pnpm test`, `pnpm build`, and `cargo clippy` inside `src-tauri`.

Anything that touches protocol behavior (sending, receiving, invites,
membership, attachments) must also pass the end-to-end suite against a
**local** relay:

```sh
docker run -d --name moyu-e2e-relay -p 127.0.0.1:7777:8080 scsibug/nostr-rs-relay:0.10.0
cargo build -p moyu-cli
bash scripts/e2e-local.sh
```

Never test against public relays: `moyu init` publishes a KeyPackage, and a
test identity published there stays there.

## What must never be committed

This repository is public, and a pushed commit cannot be taken back.

- Secrets and runtime data: keys, tokens, passphrases, an `nsec`, identity
  files, MLS/SQLCipher databases, `moyu-state.json`.
- Personal details: absolute paths under a home directory, machine names,
  personal e-mail addresses, domains or servers you own. Use `<data-dir>`,
  `~/…`, `example.com`, or a placeholder user name.
- Other people's real identifiers (a real person's npub, for instance).
  Generate throwaway keys for tests.

`scripts/privacy-check.sh` checks for all of this (gitleaks with the rules in
`.gitleaks.toml`). Turn it on as a pre-commit hook once per clone:

```sh
git config core.hooksPath .githooks
```

CI runs the same script on every pull request.

## Code conventions

- Human-readable output goes through `hprintln!` / `heprintln!` (they strip
  terminal escapes and bidi controls from text other members control); a bare
  `println!` fails the build. `--json` output goes through `output::emit*`.
- `moyu-cli` drives `moyu-core`; it does not depend on MDK crates directly.
- Comments explain why the code is the way it is. Leave out ticket numbers,
  milestone names and who asked for what.
- MDK is a pinned git dependency. Changing the pin is a reviewed step with an
  upgrade note in `CHANGELOG.md` (MDK's database migrations are forward-only).
- Licensing: MIT-compatible code only. Nothing from `whitenoise-rs` (AGPL)
  may be copied or linked. If you adapt MIT code from elsewhere, add its
  notice to `THIRD-PARTY-NOTICES.md`.
- GitHub workflows: pin actions to commit SHAs, never interpolate
  `${{ github.* }}` into a `run:` script, and never add a self-hosted runner.
