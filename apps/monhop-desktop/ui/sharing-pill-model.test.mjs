import assert from "node:assert/strict";
import test from "node:test";

import {
  INHALE_LOW,
  ORBIT_MAX_STEP,
  coastRest,
  loopAngle,
  orbitMotion,
  orbitSpeed,
  orbitStep,
  pillChange,
  pillLook,
  pillWords,
  restAngle,
  tweenTiming,
} from "./sharing-pill-model.mjs";

const TURN = 2 * Math.PI;

test("switched on, the capsule is connecting until the session is live", () => {
  assert.equal(pillLook({ inUse: true }).state, "connecting");
  assert.equal(pillLook({ inUse: true, live: true }).state, "active");
  assert.equal(pillLook({ live: true }).state, "idle");
  assert.equal(pillLook({ inUse: true, hover: true }).pause, true);
  assert.equal(pillLook({ inUse: true, live: true, hover: true }).pause, true);
});

test("hover and focus reshape the capsule only when a press would act", () => {
  assert.deepEqual(pillLook({ inUse: false, hover: true }), {
    state: "idle",
    busy: false,
    pause: false,
    lean: true,
  });
  assert.equal(pillLook({ inUse: true, focus: true }).pause, true);
  assert.equal(pillLook({ inUse: true, hover: true, busy: true }).pause, false);
  assert.equal(pillLook({ inUse: false, hover: true, busy: true }).lean, false);
});

test("right after a press the capsule shows the result, not the next action", () => {
  assert.equal(pillLook({ inUse: true, hover: true, rested: true }).pause, false);
  assert.equal(pillLook({ inUse: false, focus: true, rested: true }).lean, false);
  assert.equal(pillLook({ inUse: true, hover: true, rested: false }).pause, true);
});

test("a render that repeats the look changes nothing, so no motion restarts", () => {
  const live = pillLook({ inUse: true });
  assert.equal(pillChange(live, pillLook({ inUse: true })), null);
  assert.deepEqual(pillChange(null, live), { animate: false, inhale: false });
});

test("the capsule inhales only when its words change", () => {
  const idle = pillLook();
  const lean = pillLook({ hover: true });
  const busy = pillLook({ busy: true });
  const connecting = pillLook({ inUse: true });
  const live = pillLook({ inUse: true, live: true });
  const pause = pillLook({ inUse: true, live: true, hover: true });
  assert.deepEqual(pillChange(idle, connecting), { animate: true, inhale: true });
  assert.equal(pillChange(connecting, live).inhale, true);
  assert.equal(pillChange(live, pause).inhale, true);
  assert.equal(pillChange(pause, idle).inhale, true);
  assert.equal(pillChange(idle, lean).inhale, false);
  assert.equal(pillChange(idle, busy).inhale, false);
  assert.ok(INHALE_LOW > 0 && INHALE_LOW < 1);
});

test("the words follow the state and Pause", () => {
  assert.equal(pillWords(pillLook()), "start");
  assert.equal(pillWords(pillLook({ busy: true })), "start");
  assert.equal(pillWords(pillLook({ inUse: true })), "connecting");
  assert.equal(pillWords(pillLook({ inUse: true, live: true })), "sharing");
  assert.equal(pillWords(pillLook({ inUse: true, hover: true })), "pause");
  assert.equal(pillWords(pillLook({ inUse: true, live: true, hover: true })), "pause");
});

test("the pair spins fast while busy or connecting, slowly while live, and rests otherwise", () => {
  assert.deepEqual(orbitMotion(pillLook({ busy: true })), { lap: "--loop-orbit-busy", rest: null });
  assert.deepEqual(orbitMotion(pillLook({ inUse: true, busy: true })).lap, "--loop-orbit-busy");
  assert.deepEqual(orbitMotion(pillLook({ inUse: true })), {
    lap: "--loop-orbit-busy",
    rest: null,
  });
  assert.deepEqual(orbitMotion(pillLook({ inUse: true, live: true })), {
    lap: "--loop-orbit",
    rest: null,
  });
  // Idle rests on whole turns so each computer keeps its side; Pause on half turns.
  assert.deepEqual(orbitMotion(pillLook()), { lap: null, rest: TURN });
  assert.deepEqual(orbitMotion(pillLook({ hover: true })), { lap: null, rest: TURN });
  assert.deepEqual(orbitMotion(pillLook({ inUse: true, focus: true })), {
    lap: null,
    rest: Math.PI,
  });
});

test("a lap converts to a speed and a running loop back to its angle", () => {
  assert.equal(orbitSpeed(1000), TURN);
  assert.equal(orbitSpeed(0), 0);
  assert.equal(loopAngle(1, 250, 1000), 1 + TURN / 4);
  assert.equal(loopAngle(1, 1250, 1000), 1 + TURN / 4);
  assert.equal(restAngle(3.3, Math.PI), Math.PI);
  assert.equal(restAngle(3.3, TURN), TURN);
});

function run(state, goal, seconds) {
  let next = { ...state, done: false };
  for (let t = 0; t < seconds && !next.done; t += 1 / 60) next = orbitStep(next, goal, 1 / 60);
  return next;
}

test("spinning up eases toward the lap's speed, then hands off at exactly that speed", () => {
  const target = orbitSpeed(1000);
  const first = orbitStep({ angle: 0, speed: 0 }, { target, rest: null }, 1 / 60);
  assert.ok(first.speed > 0 && first.speed < target / 5);
  assert.equal(first.done, false);
  const steady = run({ angle: 0, speed: 0 }, { target, rest: null }, 3);
  assert.equal(steady.done, true);
  assert.equal(steady.speed, target);
});

test("slowing from a fast spin to a slow one never stops the pair", () => {
  let state = { angle: 0, speed: orbitSpeed(700), done: false };
  const target = orbitSpeed(5000);
  while (!state.done) {
    state = orbitStep(state, { target, rest: null }, 1 / 60);
    assert.ok(state.speed >= target);
  }
});

test("from any spin the pair comes to rest on a well, keeping each computer's side when idle", () => {
  for (const speed of [orbitSpeed(700), orbitSpeed(5000), -3, 0.5, 0])
    for (const angle of [0.3, 2, Math.PI, 4, 9]) {
      const idle = run({ angle, speed }, { target: 0, rest: TURN }, 5);
      assert.equal(idle.done, true);
      assert.equal(idle.speed, 0);
      assert.ok(Math.abs(idle.angle / TURN - Math.round(idle.angle / TURN)) < 1e-12);
      const pause = run({ angle, speed }, { target: 0, rest: Math.PI }, 5);
      assert.equal(pause.done, true);
      assert.ok(Math.abs(pause.angle / Math.PI - Math.round(pause.angle / Math.PI)) < 1e-12);
    }
});

test("a coast stops at the well nearest where its momentum carries it", () => {
  // A fast spin carries on forward past the nearest well rather than snapping back to it.
  const speed = orbitSpeed(700);
  assert.equal(coastRest(1.2, speed, Math.PI), Math.PI);
  assert.equal(coastRest(1.2, 0, Math.PI), 0);
  const stop = run({ angle: 1.2, speed }, { target: 0, rest: Math.PI }, 5);
  assert.equal(stop.angle, Math.PI);
  // Sitting on Pause's half turn when idle takes over, it still settles instead of balancing there.
  const idle = run({ angle: Math.PI, speed: 0 }, { target: 0, rest: TURN }, 3);
  assert.equal(idle.done, true);
  assert.ok(ORBIT_MAX_STEP > 0 && ORBIT_MAX_STEP <= 0.05);
});

test("shapes spring, colours fade, the glow blooms and an incoming label starts a beat late", () => {
  assert.deepEqual(tweenTiming({ part: "arm", property: "transform", rising: false }), {
    duration: "--motion-spring",
    easing: "--ease-spring",
    delay: null,
  });
  assert.equal(tweenTiming({ part: "capsule", property: "width" }).easing, "--ease-spring");
  assert.equal(tweenTiming({ part: "dot", property: "backgroundColor" }).duration, "--motion-slow");
  assert.equal(
    tweenTiming({ part: "glow", property: "opacity", rising: true }).duration,
    "--motion-bloom",
  );
  assert.equal(
    tweenTiming({ part: "label", property: "filter", rising: false }).duration,
    "--motion-spring",
  );
  for (const property of ["opacity", "transform", "filter"]) {
    assert.equal(tweenTiming({ part: "label", property, rising: true }).delay, "--motion-stagger");
    assert.equal(tweenTiming({ part: "label", property, rising: false }).delay, null);
  }
});

test("every timing token exists in styles.css", async () => {
  const { readFile } = await import("node:fs/promises");
  const css = await readFile(new URL("styles.css", import.meta.url), "utf8");
  const parts = ["capsule", "halo", "edge", "glow", "words", "arm", "dot", "label"];
  const properties = ["width", "opacity", "transform", "filter", "backgroundColor", "boxShadow"];
  for (const part of parts)
    for (const property of properties)
      for (const rising of [true, false]) {
        const timing = tweenTiming({ part, property, rising });
        for (const token of [timing.duration, timing.easing, timing.delay].filter(Boolean))
          assert.match(css, new RegExp(`\\n\\s*${token}:`), token);
      }
  for (const token of ["--loop-orbit", "--loop-orbit-busy", "--loop-breathe", "--motion-inhale"])
    assert.match(css, new RegExp(`\\n\\s*${token}:`), token);
});
