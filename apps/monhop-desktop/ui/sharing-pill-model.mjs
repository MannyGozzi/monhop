// What the Link capsule shows. Hover and keyboard focus reshape it only when a press would act:
// idle, the two computer nodes lean in; live, the dot and label become Pause. Right after a press
// (`rested`) it shows the result instead of the next action.
export function pillLook({
  inUse = false,
  busy = false,
  hover = false,
  focus = false,
  rested = false,
} = {}) {
  const state = inUse ? "active" : "idle";
  const engaged = !busy && !rested && (hover || focus);
  return { state, busy, pause: state === "active" && engaged, lean: state === "idle" && engaged };
}

// Null when the capsule already shows `next`, so a render never restarts its motion. The first look
// lands at once, and the light sweep crosses only when the pair goes live.
export function pillChange(shown, next) {
  if (shown && LOOK_KEYS.every((key) => shown[key] === next[key])) return null;
  return { animate: shown !== null, sweep: shown?.state === "idle" && next.state === "active" };
}

const LOOK_KEYS = ["state", "busy", "pause", "lean"];

// The edge orbit turns while it can be seen: live, or busy as the progress arc.
export function orbitShown(look) {
  return look.state === "active" || look.busy;
}

const SURFACES = new Set(["halo", "tint", "edge", "ring", "orbit", "mark"]);

// Token names for one tweened value. Width glides without overshoot, glyphs spring, surfaces fade
// slowly; the nodes fade only once they have met, the dot appears as they do, and an incoming label
// waits for the outgoing one to clear so the two never overlap.
export function tweenTiming({ part, property, rising }) {
  if (property === "width")
    return { duration: "--motion-slow", easing: "--ease-glide", delay: null };
  const merge =
    (part === "dot" && rising) || (part === "node" && !rising && property !== "transform");
  const delay = merge ? "--motion-fast" : part === "label" && rising ? "--motion-instant" : null;
  if (property === "transform")
    return { duration: "--motion-spring", easing: "--ease-spring", delay };
  if (SURFACES.has(part)) return { duration: "--motion-slow", easing: "--ease-out", delay: null };
  return { duration: rising ? "--motion-base" : "--motion-fast", easing: "--ease-out", delay };
}

// The orbit runs along the capsule's edge at one speed. Its keyframes move a point out from the
// capsule's centre: straight edges translate, round ends turn, and each keyframe's offset is its share
// of the perimeter. WebKit resolves the % translations once, when it composites the animation, so a
// width glide rebuilds the path each frame (computer-card.mjs followWidth).
export function orbitPath({ width, height, inset }) {
  const radius = height / 2 - inset;
  const straight = Math.max(width - height, 0);
  const arc = Math.PI * radius;
  const perimeter = 2 * (straight + arc);
  const end = (side) => `translateX(calc(${50 * side}% ${side < 0 ? "+" : "-"} ${height / 2}px))`;
  const at = (side, turn) => `${end(side)} rotate(${turn}turn) translateY(${-radius}px)`;
  const marks = [0, straight, straight + arc, 2 * straight + arc, perimeter];
  const points = [at(-1, 0), at(1, 0), at(1, 0.5), at(-1, 0.5), at(-1, 1)];
  return {
    perimeter,
    keyframes: points.map((transform, index) => ({ offset: marks[index] / perimeter, transform })),
  };
}

export const COMET_DOTS = 40;
const COMET_TAPER = 0.5;

// The comet's dots, head first: each trails the head by an even share of `tail` along the edge and
// fades and narrows toward the end. `lag` is a fraction of one lap.
export function cometDot(index, { perimeter, tail }) {
  const along = index / COMET_DOTS;
  return {
    lag: (tail * along) / perimeter,
    opacity: (1 - along) ** 2,
    scale: 1 - along * COMET_TAPER,
  };
}
