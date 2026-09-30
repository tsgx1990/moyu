import { describe, expect, it, vi } from "vitest";

// session.ts imports the Tauri bridge at module load; stub it so the pure
// helpers can be exercised in node.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

import { relaysFor } from "./session";

describe("relaysFor", () => {
  it("omits the relay arg for the default preset so the sidecar resolves its own", () => {
    expect(relaysFor({ preset: "default" })).toBeUndefined();
  });
  it("passes custom URLs through verbatim (self-hosted relays)", () => {
    expect(relaysFor({ preset: "custom", urls: ["wss://relay.example.com"] })).toEqual([
      "wss://relay.example.com",
    ]);
  });
});
