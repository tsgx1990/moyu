#!/usr/bin/env node
// One-click local packaging for the moyu desktop app — builds the release
// sidecar + frontend + Tauri bundle for the CURRENT platform and prints the
// installable artifacts (macOS .dmg / Linux .deb + .AppImage / Windows NSIS
// .exe). CI equivalent: .github/workflows/release-desktop.yml (app-v* tags).
//
//   node scripts/package-desktop.mjs            # release package
//   node scripts/package-desktop.mjs --debug    # fast debug package
//
// Signing policy: ALWAYS unsigned — no Apple Developer / Authenticode
// account is used or required. Any signing credentials found in the
// environment are stripped before the build so a machine that happens to
// have them can never sign by accident. On macOS the bundle is ad-hoc
// signed (tauri.conf.json `signingIdentity: "-"`): that is a free,
// account-less local signature Apple Silicon requires just to LAUNCH a
// binary — it is not a distribution signature, and Gatekeeper treatment of
// downloaded copies is unchanged (see docs/desktop-install.md).
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const appDir = path.join(repoRoot, "apps", "desktop");
const debug = process.argv.includes("--debug");

// --- environment hygiene -------------------------------------------------
const env = { ...process.env };

// Signing credentials: strip every variable the Tauri bundler (or its mac /
// Windows signing hooks) would pick up. Unsigned packaging is a hard
// requirement, not a default.
const SIGNING_VARS = [
  "APPLE_CERTIFICATE",
  "APPLE_CERTIFICATE_PASSWORD",
  "APPLE_SIGNING_IDENTITY",
  "APPLE_ID",
  "APPLE_PASSWORD",
  "APPLE_TEAM_ID",
  "APPLE_API_ISSUER",
  "APPLE_API_KEY",
  "APPLE_API_KEY_PATH",
  "TAURI_SIGNING_PRIVATE_KEY",
  "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
  "WINDOWS_CERTIFICATE",
  "WINDOWS_CERTIFICATE_PASSWORD",
  // Azure Trusted Signing (the planned Windows signing mechanism, see
  // docs/desktop-install.md) — inert until bundle.windows.signCommand
  // exists, stripped now so that rollout can never flip this script signed.
  "AZURE_CLIENT_ID",
  "AZURE_TENANT_ID",
  "AZURE_CLIENT_SECRET",
];
for (const v of SIGNING_VARS) {
  if (env[v] !== undefined) {
    console.warn(`[package-desktop] stripping ${v} from env (unsigned packaging)`);
    delete env[v];
  }
}

// A global RUSTFLAGS would silently override the target-scoped rustflags in
// the root .cargo/config.toml — on Windows that drops the /STACK:8MiB
// link-arg and ships a moyu.exe that stack-overflows on launch (see
// .cargo/config.toml). Refuse to package with it set.
if (env.RUSTFLAGS !== undefined) {
  console.warn(
    "[package-desktop] RUSTFLAGS is set; dropping it for this build — it would\n" +
      "override .cargo/config.toml's target-scoped flags (Windows /STACK fix).",
  );
  delete env.RUSTFLAGS;
}

if (debug) env.MOYU_SIDECAR_PROFILE = "debug";

// --- build ----------------------------------------------------------------
const run = (cmd, args, cwd) => {
  console.log(`\n[package-desktop] ${cmd} ${args.join(" ")}`);
  // shell:true on Windows only — `pnpm` there is a .cmd shim, which Node's
  // spawn cannot execute directly (documented child_process behavior; a bare
  // "pnpm" throws ENOENT/EINVAL). Args here are fixed flags, so the shell
  // quoting hazard doesn't apply.
  const r = spawnSync(cmd, args, {
    cwd,
    stdio: "inherit",
    env,
    shell: process.platform === "win32",
  });
  if (r.status !== 0) {
    if (r.error) console.error(`[package-desktop] spawn error: ${r.error.message}`);
    console.error(`[package-desktop] FAILED: ${cmd} ${args.join(" ")}`);
    process.exit(r.status ?? 1);
  }
};

run("pnpm", ["install", "--frozen-lockfile"], appDir);
// `tauri build` runs beforeBuildCommand (`pnpm sidecar && pnpm build`):
// release sidecar staged by scripts/build-sidecar.mjs + vite production build.
run("pnpm", ["tauri", "build", ...(debug ? ["--debug"] : [])], appDir);

// --- report artifacts -------------------------------------------------------
// The desktop workspace's cargo honors CARGO_TARGET_DIR like any other.
const targetDir = env.CARGO_TARGET_DIR ?? path.join(appDir, "src-tauri", "target");
const bundleDir = path.join(targetDir, debug ? "debug" : "release", "bundle");

const artifacts = [];
const walk = (dir) => {
  for (const name of readdirSync(dir)) {
    const p = path.join(dir, name);
    if (statSync(p).isDirectory()) walk(p);
    else if (/\.(dmg|AppImage|deb|exe|app\.tar\.gz)$/i.test(name)) artifacts.push(p);
  }
};
if (existsSync(bundleDir)) walk(bundleDir);

console.log("\n[package-desktop] done — unsigned artifacts:");
if (artifacts.length === 0) {
  console.log(`  (none matched under ${bundleDir} — inspect that directory)`);
} else {
  for (const a of artifacts) console.log(`  ${a}`);
}
if (process.platform === "darwin") {
  try {
    const appBundle = readdirSync(path.join(bundleDir, "macos"), { withFileTypes: true })
      .find((e) => e.name.endsWith(".app"));
    if (appBundle) {
      // codesign -dv reports on stderr; expect "Signature=adhoc" and no
      // Authority= lines (that would mean an account-backed identity).
      const r = spawnSync(
        "codesign",
        ["-dv", path.join(bundleDir, "macos", appBundle.name)],
        { encoding: "utf8" },
      );
      const info = (r.stderr || "").split("\n").slice(0, 6).join("\n");
      console.log(`\n[package-desktop] codesign check (expect ad-hoc, no Authority):\n${info}`);
    }
  } catch {
    /* diagnostic only */
  }
  console.log(
    "[package-desktop] 注意:本地构建的 .app 可直接运行;分发给他人时对方会遇到\n" +
      "Gatekeeper 提示(未签名),绕行说明见 docs/desktop-install.md。",
  );
}
