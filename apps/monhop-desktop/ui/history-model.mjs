// Back and forward through what the user selected, as a browser does: a new pick drops what lay
// ahead. Mouse back and forward buttons and a two-finger horizontal swipe step through it.

export const TRAIL_LIMIT = 50;
// Horizontal travel, in wheel pixels, that one swipe needs before it steps.
export const SWIPE_DISTANCE = 120;
// A pause this long between wheel events ends a swipe; momentum events arrive closer together.
export const SWIPE_QUIET_MS = 220;
// DOM mouse buttons 3 and 4 are the side buttons.
export const MOUSE_STEPS = Object.freeze({ 3: "back", 4: "forward" });

export function emptyTrail() {
  return { back: [], forward: [] };
}

export function visit(trail, from, to) {
  if (!from || !to || from === to) return trail;
  return { back: [...trail.back, from].slice(-TRAIL_LIMIT), forward: [] };
}

// The nearest selection in `direction` that can still be shown; ones that cannot are dropped.
export function step(trail, current, direction, canShow) {
  const forwards = direction === "forward";
  const ahead = [...(forwards ? trail.forward : trail.back)];
  const behind = [...(forwards ? trail.back : trail.forward)];
  while (ahead.length) {
    const key = ahead.pop();
    if (key === current || !canShow(key)) continue;
    behind.push(current);
    const next = forwards ? { back: behind, forward: ahead } : { back: ahead, forward: behind };
    return { trail: next, key };
  }
  return { trail, key: null };
}

// One burst of wheel events steps at most once. A burst that starts vertical stays a scroll, and
// travel toward the left (negative deltaX) goes back, as in Safari and Edge.
export function trackSwipe(swipe, { dx, dy, at }) {
  const fresh = swipe === null || at - swipe.at > SWIPE_QUIET_MS;
  const current = fresh ? { axis: null, travel: 0, done: false, at } : { ...swipe, at };
  if (current.axis === null && (dx !== 0 || dy !== 0))
    current.axis = Math.abs(dx) > Math.abs(dy) ? "x" : "y";
  if (current.done || current.axis !== "x") return { swipe: current, direction: null };
  current.travel += dx;
  if (Math.abs(current.travel) < SWIPE_DISTANCE) return { swipe: current, direction: null };
  current.done = true;
  return { swipe: current, direction: current.travel < 0 ? "back" : "forward" };
}
