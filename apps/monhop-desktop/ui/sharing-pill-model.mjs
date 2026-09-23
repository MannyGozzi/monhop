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
