#!/usr/bin/env node
// Build moyu-cli and stage it as the Tauri sidecar binary at
// apps/desktop/src-tauri/binaries/moyu-<host-triple>[.exe] (Tauri resolves
// `externalBin: ["binaries/moyu"]` to exactly that name). Building from this
// same checkout means the GUI and its bundled CLI can never drift — version
// lockstep by construction (desktop-shell plan §C2).
//
// Env knobs:
//   MOYU_SIDECAR_PATH    — skip the build, stage this prebuilt moyu binary
//   MOYU_SIDECAR_PROFILE — "debug" for fast dev iteration (default "release")
//   MOYU_SIDECAR_TARGET  — cross-build for this target triple (`cargo --target`)
//                          and stage binaries/moyu-<that triple>. Empty/unset
//                          = fall back to TAURI_ENV_TARGET_TRIPLE (the Tauri
//                          CLI sets it for beforeBuildCommand, so
//                          `tauri build --target X` re-invoking this script
//                          via `pnpm sidecar` builds the same X — a cargo
//                          no-op after the workflow's own sidecar step —
//                          instead of a wasted host build), then to the host.
//                          Used by release-desktop.yml for the x86_64 macOS
//                          leg, which runs on the Apple Silicon self-hosted
//                          runner.
//   CARGO_TARGET_DIR     — honored, same as cargo itself
import { execFileSync } from "node:child_process";
import { copyFileSync, chmodSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const exe = process.platform === "win32" ? ".exe" : "";

const hostLine = execFileSync("rustc", ["-vV"])
  .toString()
  .split("\n")
  .find((l) => l.startsWith("host:"));
if (!hostLine) throw new Error("cannot determine host triple from `rustc -vV`");
const host = hostLine.split(":")[1].trim();
const triple = process.env.MOYU_SIDECAR_TARGET || process.env.TAURI_ENV_TARGET_TRIPLE || host;
const cross = triple !== host;

let src = process.env.MOYU_SIDECAR_PATH;
if (!src) {
  const profile = process.env.MOYU_SIDECAR_PROFILE === "debug" ? "debug" : "release";
  const args = ["build", "-p", "moyu-cli"];
  if (profile === "release") args.push("--release");
  if (cross) args.push("--target", triple);
  execFileSync("cargo", args, { cwd: repoRoot, stdio: "inherit", env: process.env });
  const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");
  // cargo nests explicit-target output under target/<triple>/.
  src = path.join(targetDir, ...(cross ? [triple] : []), profile, `moyu${exe}`);
}

const destDir = path.join(repoRoot, "apps", "desktop", "src-tauri", "binaries");
mkdirSync(destDir, { recursive: true });
const dest = path.join(destDir, `moyu-${triple}${exe}`);
copyFileSync(src, dest);
chmodSync(dest, 0o755);
console.log(`sidecar staged: ${dest}`);
