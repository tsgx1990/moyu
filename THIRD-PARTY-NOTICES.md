# Third-party notices

moyu is MIT-licensed (see [`LICENSE`](LICENSE)). It builds on the projects
below. Rust and npm dependencies are pulled in unmodified at build time and
are listed in `Cargo.lock`, `apps/desktop/src-tauri/Cargo.lock` and
`apps/desktop/pnpm-lock.yaml`; `cargo deny` (see `deny.toml`) enforces an SPDX
license allowlist over the Rust ones and rejects the GPL family.

## MDK (Marmot Development Kit)

<https://github.com/marmot-protocol/mdk> — MIT.

moyu links MDK's crates (through the fork at <https://github.com/tsgx1990/mdk>,
which adds SOCKS5 proxy support to upstream), and the terminal setup/teardown
code in `crates/moyu-cli/src/tui.rs` follows the pattern of MDK's own TUI.

```
MIT License

Copyright (c) 2024-2026 Internet Privacy Foundation

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Not used

No code from `whitenoise-rs` (AGPL-3.0) is included in or linked by moyu.
