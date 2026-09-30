import { describe, expect, it, vi } from "vitest";
import { makeCatchupTrigger } from "./catchup";

describe("makeCatchupTrigger", () => {
  it("fires on the leading edge and swallows repeats inside the window", () => {
    let clock = 1_000;
    const fn = vi.fn();
    const trigger = makeCatchupTrigger(fn, 10_000, () => clock);
    expect(trigger()).toBe(true);
    clock += 3_000;
    expect(trigger()).toBe(false);
    clock += 6_999;
    expect(trigger()).toBe(false);
    expect(fn).toHaveBeenCalledTimes(1);
  });

  it("fires again once the window has elapsed", () => {
    let clock = 0;
    const fn = vi.fn();
    const trigger = makeCatchupTrigger(fn, 10_000, () => clock);
    trigger();
    clock += 10_000;
    expect(trigger()).toBe(true);
    expect(fn).toHaveBeenCalledTimes(2);
  });
});
