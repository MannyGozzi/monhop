// The first of `candidates` the engine can parse as an easing, so Web Animations never throws on a
// token it does not know: linear() reached WebKit in Safari 17.2, and macOS 14.0 ships 17.0.
export function usableEasing(candidates, supports) {
  return candidates.find((value) => Boolean(value) && supports(value)) ?? "ease-out";
}

// Opacity steps smaller than this are not drawn differently.
const OPACITY_EPSILON = 0.01;

// How a control a render rebuilt takes over its predecessor's look: "carry" keeps a fade still running
// toward the same opacity, "fade" eases from what was drawn to the new resting opacity, null leaves it.
export function dimFollow({ drawn, resting, running = null }) {
  if (running !== null && Math.abs(running - resting) < OPACITY_EPSILON) return "carry";
  return Math.abs(drawn - resting) < OPACITY_EPSILON ? null : "fade";
}

// The value a tween from `from` to `to` shows at eased `progress` (null once it has ended).
export function tweenValue(from, to, progress) {
  return progress === null ? to : from + (to - from) * progress;
}

// Where a value easing toward `target` stands `dt` ms later: it closes about 98% of the gap in
// `settle` ms at any frame rate, so a target that moves meanwhile is followed without a jump. Each step
// moves at least `quantum` (a scroll offset snaps to device pixels and would stall short of the target).
export function followStep(current, target, dt, settle, quantum = 0) {
  const gap = target - current;
  if (!(settle > 0) || Math.abs(gap) <= Math.max(quantum, 0.5)) return target;
  const step = gap * (1 - Math.exp((-4 * dt) / settle));
  return current + Math.sign(gap) * Math.max(Math.abs(step), quantum);
}

// A busy glyph's turn on the document clock (`now`, ms): it starts from rest when work starts, keeps
// its phase across rebuilds, and once idle finishes the turn it is on rather than snapping upright.
// `startedAt` and `endsAt` place the loop; `turning` says whether the glyph still spins.
export function spinTurn(last, { busy, now, period }) {
  const running = Boolean(last) && (last.busy || now < last.endsAt);
  const startedAt = running ? last.startedAt : now;
  if (busy) return { busy, startedAt, endsAt: Infinity, turning: true };
  if (!running || !(period > 0))
    return { busy, startedAt: null, endsAt: -Infinity, turning: false };
  const endsAt = last.busy
    ? startedAt + period * Math.max(1, Math.ceil((now - startedAt) / period))
    : last.endsAt;
  return { busy, startedAt, endsAt, turning: now < endsAt };
}
