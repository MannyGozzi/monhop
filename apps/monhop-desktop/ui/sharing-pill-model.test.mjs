import assert from "node:assert/strict";
import test from "node:test";

import {
  COMET_DOTS,
  cometDot,
  orbitPath,
  orbitShown,
  pillChange,
  pillLook,
  tweenTiming,
} from "./sharing-pill-model.mjs";

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

test("the orbit spends time on each stretch of edge in proportion to its length", () => {
  const { perimeter, keyframes } = orbitPath({ width: 90, height: 30, inset: 0.75 });
  const straight = 60;
  const arc = Math.PI * 14.25;
  assert.ok(Math.abs(perimeter - 2 * (straight + arc)) < 1e-9);
  const offsets = keyframes.map((frame) => frame.offset);
  const expected = [0, straight, straight + arc, 2 * straight + arc, perimeter].map(
    (mark) => mark / perimeter,
  );
  offsets.forEach((offset, index) => assert.ok(Math.abs(offset - expected[index]) < 1e-9));
  // Each stretch pairs one moving component: the straights translate, the ends turn.
  assert.equal(
    keyframes[0].transform,
    "translateX(calc(-50% + 15px)) rotate(0turn) translateY(-14.25px)",
  );
  assert.equal(
    keyframes[2].transform,
    "translateX(calc(50% - 15px)) rotate(0.5turn) translateY(-14.25px)",
  );
  assert.equal(
    keyframes[4].transform,
    "translateX(calc(-50% + 15px)) rotate(1turn) translateY(-14.25px)",
  );
});

test("a round capsule has no straight stretch", () => {
  const { keyframes } = orbitPath({ width: 30, height: 30, inset: 0 });
  assert.equal(keyframes[1].offset, 0);
  assert.equal(keyframes[2].offset, 0.5);
});

test("the comet trails its head by an even share of the tail and fades out", () => {
  const geometry = { perimeter: 200, tail: 50 };
  const dots = Array.from({ length: COMET_DOTS }, (_, index) => cometDot(index, geometry));
  assert.deepEqual(dots[0], { lag: 0, opacity: 1, scale: 1 });
  const step = dots[1].lag - dots[0].lag;
  for (let index = 1; index < COMET_DOTS; index += 1) {
    assert.ok(Math.abs(dots[index].lag - dots[index - 1].lag - step) < 1e-12);
    assert.ok(dots[index].opacity < dots[index - 1].opacity);
    assert.ok(dots[index].scale < dots[index - 1].scale);
  }
  assert.ok(dots.at(-1).lag < 50 / 200);
});
