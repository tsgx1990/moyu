import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

import { buildConvs, dedupeAppend, rowInConv, type Conversation } from "./store";
import type { Conversations, MessageRow } from "./lib/session";

const row = (over: Partial<MessageRow>): MessageRow => ({
  event: "message",
  group: "g1",
  sender: "s",
  ts: 1,
  message_id: "m1",
  ...over,
});

const channelConv: Conversation = {
  key: "ch:g1:eng",
  kind: "channel",
  title: "eng",
  group: "g1",
  channel: "eng",
  unread: 0,
};

describe("rowInConv", () => {
  it("rejects rows from another MLS group outright", () => {
    expect(rowInConv(row({ group: "g2", channel: "eng" }), channelConv)).toBe(false);
  });
  it("filters a public channel by slug, treating a missing slug as #general", () => {
    expect(rowInConv(row({ channel: "eng" }), channelConv)).toBe(true);
    expect(rowInConv(row({ channel: "ops" }), channelConv)).toBe(false);
    const general: Conversation = { ...channelConv, key: "ch:g1:general", channel: "general" };
    expect(rowInConv(row({}), general)).toBe(true);
  });
  it("lets reactions through regardless of channel (they are keyed by target)", () => {
    expect(rowInConv(row({ event: "reaction", channel: "ops", emoji: "👍" }), channelConv)).toBe(
      true,
    );
  });
  it("matches a DM or private channel purely by group", () => {
    const dm: Conversation = { key: "dm:g1", kind: "dm", title: "bob", group: "g1", unread: 0 };
    expect(rowInConv(row({ channel: "whatever" }), dm)).toBe(true);
  });
});

describe("dedupeAppend", () => {
  it("appends a new message and ignores a redelivered one", () => {
    const first = dedupeAppend([], row({ message_id: "a" }));
    expect(first.map((r) => r.message_id)).toEqual(["a"]);
    const again = dedupeAppend(first, row({ message_id: "a", body: "redelivered" }));
    expect(again).toBe(first);
    expect(dedupeAppend(first, row({ message_id: "b" })).map((r) => r.message_id)).toEqual([
      "a",
      "b",
    ]);
  });
});

describe("buildConvs", () => {
  const view: Conversations = {
    dms: [
      { group: "d1", npub: "npub1abcdefghijklmnopqrstuvwxyz", label: "alice" },
      { group: "d2", npub: "npub1zyxwvutsrqponmlkjihgfedcba" },
    ],
    workspaces: [
      {
        group: "w1",
        name: "acme",
        channels: [
          { slug: "general", name: "general", private: false, archived: false, group: null },
          { slug: "old", name: "old", private: false, archived: true, group: null },
          { slug: "secret", name: "secret", private: true, archived: false, group: "p1" },
        ],
      },
    ],
    contacts: [],
  };

  it("lists DMs first, labelled by contact label or a shortened npub", () => {
    const convs = buildConvs(view);
    expect(convs[0]).toMatchObject({ key: "dm:d1", kind: "dm", title: "alice", peerNpub: view.dms[0].npub });
    expect(convs[1]).toMatchObject({ key: "dm:d2", kind: "dm", title: "npub1zyxwv…dcba" });
  });

  it("skips archived channels and keys private channels by their own group", () => {
    const keys = buildConvs(view).map((c) => c.key);
    expect(keys).toEqual(["dm:d1", "dm:d2", "ch:w1:general", "pc:p1"]);
    const secret = buildConvs(view).find((c) => c.key === "pc:p1")!;
    expect(secret).toMatchObject({ kind: "private", group: "p1", wsGroup: "w1", wsName: "acme" });
    const general = buildConvs(view).find((c) => c.key === "ch:w1:general")!;
    expect(general).toMatchObject({ kind: "channel", group: "w1", channel: "general" });
  });
});
