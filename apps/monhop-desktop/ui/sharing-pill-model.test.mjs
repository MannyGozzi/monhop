import assert from "node:assert/strict";
import test from "node:test";

import { orbitShown, pillChange, pillLook, tweenTiming } from "./sharing-pill-model.mjs";

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
  assert.deepEqual(pillChange(null, live), { animate: false, sweep: false });
});

test("the light sweep crosses only when the pair goes live", () => {
  const idle = pillLook();
  const busy = pillLook({ busy: true });
  const live = pillLook({ inUse: true });
  assert.deepEqual(pillChange(idle, live), { animate: true, sweep: true });
  assert.deepEqual(pillChange(busy, live), { animate: true, sweep: true });
  assert.equal(pillChange(live, idle).sweep, false);
  assert.equal(pillChange(live, pillLook({ inUse: true, hover: true })).sweep, false);
});

test("the orbit shows while live and as the busy arc", () => {
  assert.equal(orbitShown(pillLook()), false);
  assert.equal(orbitShown(pillLook({ busy: true })), true);
  assert.equal(orbitShown(pillLook({ inUse: true })), true);
});

test("the nodes glide at once and fade only after meeting, and the dot appears as they merge", () => {
  const glide = tweenTiming({ part: "node", property: "transform", rising: false });
  assert.deepEqual(glide, { duration: "--motion-spring", easing: "--ease-spring", delay: null });
  assert.equal(
    tweenTiming({ part: "node", property: "opacity", rising: false }).delay,
    "--motion-fast",
  );
  assert.equal(
    tweenTiming({ part: "dot", property: "opacity", rising: true }).delay,
    "--motion-fast",
  );
  assert.equal(
    tweenTiming({ part: "dot", property: "transform", rising: true }).delay,
    "--motion-fast",
  );
  // Splitting apart, the nodes appear at once while the dot leaves.
  assert.equal(tweenTiming({ part: "node", property: "opacity", rising: true }).delay, null);
  assert.equal(tweenTiming({ part: "dot", property: "opacity", rising: false }).delay, null);
});

test("an incoming label waits for the outgoing one to clear", () => {
  const out = tweenTiming({ part: "label", property: "opacity", rising: false });
  assert.equal(out.delay, null);
  for (const property of ["opacity", "transform"])
    assert.equal(tweenTiming({ part: "label", property, rising: true }).delay, "--motion-instant");
});

test("width glides without overshoot and surfaces fade slowly", () => {
  assert.deepEqual(tweenTiming({ part: "capsule", property: "width", rising: true }), {
    duration: "--motion-slow",
    easing: "--ease-glide",
    delay: null,
  });
  assert.equal(
    tweenTiming({ part: "tint", property: "opacity", rising: true }).duration,
    "--motion-slow",
  );
  assert.equal(
    tweenTiming({ part: "label", property: "opacity", rising: false }).duration,
    "--motion-fast",
  );
});

test("every timing token exists in styles.css", async () => {
  const { readFile } = await import("node:fs/promises");
  const css = await readFile(new URL("styles.css", import.meta.url), "utf8");
  const parts = [
    "capsule",
    "halo",
    "tint",
    "edge",
    "ring",
    "orbit",
    "mark",
    "node",
    "dot",
    "bar",
    "label",
  ];
  for (const part of parts)
    for (const property of ["width", "opacity", "transform"])
      for (const rising of [true, false]) {
        const timing = tweenTiming({ part, property, rising });
        for (const token of [timing.duration, timing.easing, timing.delay].filter(Boolean))
          assert.match(css, new RegExp(`\\n\\s*${token}:`), token);
      }
});
