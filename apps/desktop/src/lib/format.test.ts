import { describe, expect, it } from "vitest";
import { errMsg, shortHex, shortNpub } from "./format";

describe("errMsg", () => {
  it("uses an Error's message and stringifies anything else", () => {
    expect(errMsg(new Error("boom"))).toBe("boom");
    expect(errMsg("plain")).toBe("plain");
    expect(errMsg(42)).toBe("42");
    expect(errMsg(undefined)).toBe("undefined");
  });
});

describe("shortHex", () => {
  it("keeps the first 8 characters by default and never pads", () => {
    expect(shortHex("deadbeefcafebabe")).toBe("deadbeef");
    expect(shortHex("abc")).toBe("abc");
    expect(shortHex("deadbeefcafebabe", 4)).toBe("dead");
  });
});

describe("shortNpub", () => {
  it("shows head + tail so similar npubs stay distinguishable", () => {
    expect(shortNpub("npub1abcdefghijklmnopqrstuvwxyz")).toBe("npub1abcde…wxyz");
  });
  it("falls back to 'unknown' when there is no npub", () => {
    expect(shortNpub(undefined)).toBe("unknown");
    expect(shortNpub("")).toBe("unknown");
  });
});
