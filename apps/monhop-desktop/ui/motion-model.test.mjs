import assert from "node:assert/strict";
import test from "node:test";

import { dimFollow, followStep, spinTurn, tweenValue, usableEasing } from "./motion-model.mjs";

const SPRING = "linear(0, 0.106 5%, 1.038 44%, 1)";
const OUT = "cubic-bezier(.22, 1, .36, 1)";
const modern = () => true;
const beforeLinear = (value) => !value.startsWith("linear(");

test("a token the engine parses is used as is", () => {
  assert.equal(usableEasing([SPRING, OUT], modern), SPRING);
  assert.equal(usableEasing([OUT, OUT], beforeLinear), OUT);
});

test("an engine without linear() gets the next token instead of a throwing animate()", () => {
  assert.equal(usableEasing([SPRING, OUT], beforeLinear), OUT);
});

test("unreadable or rejected tokens end on a keyword every engine knows", () => {
  assert.equal(usableEasing(["", ""], modern), "ease-out");
  assert.equal(usableEasing([SPRING, SPRING], beforeLinear), "ease-out");
});

// One axis of a CSS cubic-bezier at curve parameter t, from its two inner control values.
const bezierAxis = (a, b, t) => 3 * a * t * (1 - t) ** 2 + 3 * b * t * t * (1 - t) + t ** 3;

// y of a CSS cubic-bezier at progress x, solved by bisection on its x curve.
function bezierAt([x1, y1, x2, y2], x) {
  let low = 0;
  let high = 1;
  for (let step = 0; step < 50; step++) {
    const mid = (low + high) / 2;
    if (bezierAxis(x1, x2, mid) < x) low = mid;
    else high = mid;
  }
  return bezierAxis(y1, y2, (low + high) / 2);
}

test("an engine without linear() still springs, close to the linear() curve", async () => {
  const { readFile } = await import("node:fs/promises");
  const css = await readFile(new URL("styles.css", import.meta.url), "utf8");
  const spring = css
    .match(/--ease-spring: linear\(([^)]*)\)/)[1]
    .split(",")
    .map((stop) => {
      const [value, percent] = stop.trim().split(/\s+/);
      return { value: Number(value), percent: percent ? Number.parseFloat(percent) / 100 : null };
    });
  spring[0].percent ??= 0;
  spring.at(-1).percent ??= 1;
  const fallback = css.match(
    /@supports not \(transition-timing-function: linear\(0, 1\)\) \{ :root \{ --ease-spring: cubic-bezier\(([^)]*)\); \} \}/,
  )[1];
  const curve = fallback.split(",").map(Number);
  let peak = 0;
  for (const [index, stop] of spring.entries()) {
    const fitted = bezierAt(curve, stop.percent);
    peak = Math.max(peak, fitted);
    assert.ok(Math.abs(fitted - stop.value) < 0.07, `stop ${index} at ${stop.percent}`);
  }
  assert.ok(peak > 1.02, "the fallback overshoots like the spring");
});

test("a rebuilt control fades from the opacity it was drawn at to its new resting one", () => {
  assert.equal(dimFollow({ drawn: 1, resting: 0.45 }), "fade");
  assert.equal(dimFollow({ drawn: 0.7, resting: 1 }), "fade");
});

test("a rebuilt control already at rest draws nothing new", () => {
  assert.equal(dimFollow({ drawn: 0.45, resting: 0.45 }), null);
  assert.equal(dimFollow({ drawn: 1, resting: 1, running: null }), null);
});

test("a fade still heading where the rebuilt control rests carries on instead of restarting", () => {
  assert.equal(dimFollow({ drawn: 0.6, resting: 0.45, running: 0.45 }), "carry");
});

test("a fade heading elsewhere is retargeted from what it drew", () => {
  assert.equal(dimFollow({ drawn: 0.6, resting: 1, running: 0.45 }), "fade");
});

test("a tween's value follows its eased progress and lands on its target", () => {
  assert.equal(tweenValue(0.2, 0.6, 0), 0.2);
  assert.equal(tweenValue(0.2, 0.6, 0.5), 0.4);
  assert.equal(tweenValue(0.2, 0.6, null), 0.6);
});

test("a glyph that starts work turns from rest", () => {
  const turn = spinTurn(null, { busy: true, now: 1000, period: 800 });
  assert.deepEqual(turn, { busy: true, startedAt: 1000, endsAt: Infinity, turning: true });
});

test("a rebuilt busy glyph keeps the phase it started with", () => {
  const first = spinTurn(null, { busy: true, now: 1000, period: 800 });
  assert.equal(spinTurn(first, { busy: true, now: 2500, period: 800 }).startedAt, 1000);
});

test("an idle glyph finishes the turn it is on and then rests", () => {
  const busy = spinTurn(null, { busy: true, now: 1000, period: 800 });
  const stopped = spinTurn(busy, { busy: false, now: 2000, period: 800 });
  assert.deepEqual(stopped, { busy: false, startedAt: 1000, endsAt: 2600, turning: true });
  const later = spinTurn(stopped, { busy: false, now: 2300, period: 800 });
  assert.deepEqual(later, stopped);
  assert.equal(spinTurn(later, { busy: false, now: 2600, period: 800 }).turning, false);
});

test("work that resumes mid-way through the last turn carries on without a jump", () => {
  const busy = spinTurn(null, { busy: true, now: 1000, period: 800 });
  const stopped = spinTurn(busy, { busy: false, now: 2000, period: 800 });
  const resumed = spinTurn(stopped, { busy: true, now: 2200, period: 800 });
  assert.equal(resumed.startedAt, 1000);
  assert.equal(resumed.endsAt, Infinity);
});

test("work that resumes after the glyph came to rest starts a fresh turn", () => {
  const busy = spinTurn(null, { busy: true, now: 1000, period: 800 });
  const stopped = spinTurn(busy, { busy: false, now: 2000, period: 800 });
  assert.equal(spinTurn(stopped, { busy: true, now: 3000, period: 800 }).startedAt, 3000);
});

test("a glyph that was never busy does not turn", () => {
  assert.equal(spinTurn(null, { busy: false, now: 1000, period: 800 }).turning, false);
});

test("a follow closes most of the gap within its settle time, at any frame rate", () => {
  let at60 = 0;
  for (let t = 0; t < 340; t += 1000 / 60) at60 = followStep(at60, 100, 1000 / 60, 340);
  let at120 = 0;
  for (let t = 0; t < 340; t += 1000 / 120) at120 = followStep(at120, 100, 1000 / 120, 340);
  assert.ok(at60 > 97 && at60 <= 100);
  assert.ok(Math.abs(at60 - at120) < 1);
});

test("a follow lands on its target once the gap is below a pixel, and never overshoots", () => {
  assert.equal(followStep(99.8, 100, 16, 340), 100);
  assert.ok(followStep(0, 100, 1000, 340) <= 100);
  assert.equal(followStep(0, 100, 16, 0), 100);
});

test("a follow on a snapped scale moves at least one step a frame, so it never stalls short", () => {
  assert.equal(followStep(94, 96, 8, 340, 1), 95);
  assert.equal(followStep(95, 96, 8, 340, 1), 96);
  assert.equal(followStep(96, 94, 8, 340, 1), 95);
});
