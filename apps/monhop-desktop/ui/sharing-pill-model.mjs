// What the Link capsule shows. Hover and keyboard focus reshape it only when a press would act:
// idle, the two computers lean in; live, the orbit stops into Pause's bars. Right after a press
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
// lands at once, and the capsule inhales whenever its words change.
export function pillChange(shown, next) {
  if (shown && LOOK_KEYS.every((key) => shown[key] === next[key])) return null;
  return {
    animate: shown !== null,
    inhale: shown !== null && pillWords(shown) !== pillWords(next),
  };
}

const LOOK_KEYS = ["state", "busy", "pause", "lean"];

// The label the capsule shows.
export function pillWords(look) {
  if (look.state !== "active") return "start";
  return look.pause ? "pause" : "sharing";
}

// Where the inhale bottoms out, as a share of its run.
export const INHALE_LOW = 0.35;

// Token names for one tweened value. Width and shapes spring, colours and surfaces fade, the live glow
// blooms slower still, and an incoming label starts a beat after the outgoing one so the two blur
// through each other.
export function tweenTiming({ part, property, rising }) {
  const delay = part === "label" && rising ? "--motion-stagger" : null;
  if (property === "transform" || property === "width")
    return { duration: "--motion-spring", easing: "--ease-spring", delay };
  if (part === "glow") return { duration: "--motion-bloom", easing: "--ease-out", delay: null };
  if (part === "edge") return { duration: "--motion-fast", easing: "--ease-out", delay: null };
  if (part === "label" && property === "filter")
    return { duration: "--motion-spring", easing: "--ease-out", delay };
  return { duration: "--motion-slow", easing: "--ease-out", delay };
}

const TURN = 2 * Math.PI;

// How the two computers circle: a lap token while they spin, else the turn they come to rest on.
// Idle rests on whole turns so each computer keeps its side; Pause rests on half turns, where either
// way round the pair reads as the two bars.
export function orbitMotion(look) {
  if (look.busy) return { lap: "--loop-orbit-busy", rest: null };
  if (look.state === "active" && !look.pause) return { lap: "--loop-orbit", rest: null };
  return { lap: null, rest: look.pause ? TURN / 2 : TURN };
}

// Radians per second for a lap of `lapMs`, or 0 for none.
export function orbitSpeed(lapMs) {
  return lapMs > 0 ? (TURN * 1000) / lapMs : 0;
}

// The angle of a loop that started at `from` and has run `elapsedMs` of laps `lapMs` long.
export function loopAngle(from, elapsedMs, lapMs) {
  return from + (TURN * (elapsedMs % lapMs)) / lapMs;
}

// The nearest angle the pair can rest on.
export function restAngle(angle, rest) {
  return Math.round(angle / rest) * rest;
}

// Spin-up rate (1/s); the pull onto a rest angle (1/s²) and its damping (1/s), a coast of about half a second.
const SPIN_RESPONSE = 5;
const REST_PULL = 110;
const REST_DAMPING = 16;
// Close enough to hand a spin to a compositor loop (share of its speed), or to call the pair at rest.
const STEADY_SHARE = 0.01;
const SETTLED_SPEED = 0.01;
const SETTLED_ANGLE = 0.002;
// The longest step one frame may take, so a stalled frame never flings the pair.
export const ORBIT_MAX_STEP = 1 / 30;

// Where a coasting pair comes to rest: the rest angle nearest the point its momentum would carry it
// to, so a stop never swings back across a lap it has left or balances on the far side of a well.
export function coastRest(angle, speed, rest) {
  return restAngle(angle + speed / REST_DAMPING, rest);
}

// One frame of the pair's turn, `dt` seconds long. Spinning, the speed eases toward `target`;
// resting, the pair springs onto the rest angle it chose when it began to coast (`well`). `done`
// means the turn can pass to a compositor loop (at speed) or stop (at rest).
export function orbitStep({ angle, speed, well = null }, { target, rest }, dt) {
  if (target > 0) {
    const eased = speed + (target - speed) * Math.min(1, dt * SPIN_RESPONSE);
    const done = Math.abs(target - eased) <= target * STEADY_SHARE;
    const next = done ? target : eased;
    return { angle: angle + next * dt, speed: next, well: null, done };
  }
  const stop = well ?? coastRest(angle, speed, rest);
  const nextSpeed = speed + (-REST_PULL * (angle - stop) - REST_DAMPING * speed) * dt;
  const nextAngle = angle + nextSpeed * dt;
  if (Math.abs(nextSpeed) < SETTLED_SPEED && Math.abs(nextAngle - stop) < SETTLED_ANGLE)
    return { angle: stop, speed: 0, well: stop, done: true };
  return { angle: nextAngle, speed: nextSpeed, well: stop, done: false };
}
