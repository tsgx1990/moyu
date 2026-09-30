// Leading-edge throttle for the relay catch-up trigger. Pure so it can be
// unit-tested with an injected clock; App.tsx wires it to `online` and
// `visibilitychange`.

/** Returns a trigger that calls `fn` immediately, then ignores further calls
 * until `windowMs` has elapsed since the last accepted one. The trigger
 * returns whether it fired. */
export function makeCatchupTrigger(
  fn: () => void,
  windowMs: number,
  now: () => number = Date.now,
): () => boolean {
  let last = Number.NEGATIVE_INFINITY;
  return () => {
    const t = now();
    if (t - last < windowMs) return false;
    last = t;
    fn();
    return true;
  };
}
