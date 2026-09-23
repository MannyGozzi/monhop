import assert from "node:assert/strict";
import test from "node:test";

import { usableEasing } from "./motion-model.mjs";

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
