import assert from "node:assert/strict";
import test from "node:test";

import { carriedPlace, seamPlaces } from "./arrangement-render.mjs";

const UNIT = { originX: 0, originY: 0, scale: 1 };

function seam(start, end, extra = {}) {
  return { start, end, fromDisplay: "a", toDisplay: "b", ...extra };
}

test("a crossing is placed from its top or left end on the stage, with its axis and groups", () => {
  const [vertical, horizontal] = seamPlaces(
    [
      seam([100, 80], [100, 0], { fromGroup: "local", toGroup: "peer" }),
      seam([80, 10], [0, 10], { fromDisplay: "c", toDisplay: "d" }),
    ],
    { originX: 10, originY: 20, scale: 0.5 },
  );
  assert.deepEqual(
    { ...vertical, knob: null },
    {
      key: "a>b",
      from: "a",
      to: "b",
      groups: ["local", "peer"],
      axis: "v",
      x: 60,
      y: 20,
      length: 40,
      knob: null,
    },
  );
  assert.deepEqual(vertical.knob, { x: 60, y: 40 });
  assert.equal(horizontal.axis, "h");
  assert.deepEqual([horizontal.x, horizontal.y, horizontal.length], [10, 25, 40]);
});

test("crossings between the same displays keep apart keys, and broken ones are skipped", () => {
  const places = seamPlaces(
    [
      seam([0, 0], [0, 50]),
      seam([0, 60], [0, 90]),
      seam([0, Number.NaN], [0, 10]),
      seam(null, [0, 1]),
    ],
    UNIT,
  );
  assert.deepEqual(
    places.map((place) => place.key),
    ["a>b", "a>b#1"],
  );
  assert.deepEqual(seamPlaces(null, UNIT), []);
});

test("a short crossing, or one drawn without handles, has no handle", () => {
  assert.equal(seamPlaces([seam([0, 0], [0, 29])], UNIT)[0].knob, null);
  assert.equal(seamPlaces([seam([0, 0], [0, 100])], UNIT, { handles: false })[0].knob, null);
});

test("the handle moves along its crossing off a display's marks, or goes where none is clear", () => {
  const along = seamPlaces([seam([60, 0], [60, 100])], UNIT, {
    keepClear: [{ x: 50, y: 45, width: 20, height: 10 }],
  });
  assert.deepEqual(along[0].knob, { x: 60, y: 32 });
  const covered = seamPlaces([seam([60, 0], [60, 100])], UNIT, {
    keepClear: [{ x: 0, y: 0, width: 200, height: 200 }],
  });
  assert.equal(covered[0].knob, null);
});

test("a carried crossing follows its box's position and stretch along its own axis", () => {
  const rest = { x: 50, y: 10, width: 100, height: 100 };
  const drawn = { x: 0, y: 0, width: 50, height: 200 };
  const vertical = { key: "a>b", axis: "v", x: 60, y: 20, length: 40, knob: { x: 60, y: 40 } };
  assert.deepEqual(carriedPlace(vertical, drawn, rest), {
    ...vertical,
    x: 5,
    y: 20,
    length: 80,
    knob: { x: 5, y: 60 },
  });
  const horizontal = { key: "c>d", axis: "h", x: 50, y: 110, length: 100, knob: null };
  assert.deepEqual(carriedPlace(horizontal, drawn, rest), {
    ...horizontal,
    x: 0,
    y: 200,
    length: 50,
  });
  assert.deepEqual(carriedPlace(vertical, rest, rest), vertical);
});
