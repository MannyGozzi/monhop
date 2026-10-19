import test from "node:test";
import assert from "node:assert/strict";
import {
  SWIPE_DISTANCE,
  SWIPE_QUIET_MS,
  TRAIL_LIMIT,
  emptyTrail,
  step,
  trackSwipe,
  visit,
} from "./history-model.mjs";

const always = () => true;

test("back returns to the previous pick and forward comes back", () => {
  let trail = visit(emptyTrail(), "local", "settings");
  const back = step(trail, "settings", "back", always);
  assert.equal(back.key, "local");
  trail = back.trail;
  const forward = step(trail, "local", "forward", always);
  assert.equal(forward.key, "settings");
  assert.deepEqual(forward.trail, { back: ["local"], forward: [] });
});

test("a new pick drops what lay ahead", () => {
  let trail = visit(emptyTrail(), "local", "settings");
  trail = step(trail, "settings", "back", always).trail;
  trail = visit(trail, "local", "new");
  assert.deepEqual(trail.forward, []);
  assert.equal(step(trail, "new", "forward", always).key, null);
});

test("a selection that can no longer be shown is skipped and dropped", () => {
  let trail = visit(emptyTrail(), "local", "computer:aa");
  trail = visit(trail, "computer:aa", "settings");
  const back = step(trail, "settings", "back", (key) => key !== "computer:aa");
  assert.equal(back.key, "local");
  assert.deepEqual(back.trail, { back: [], forward: ["settings"] });
});

test("nothing behind or ahead steps nowhere and keeps the trail", () => {
  const trail = emptyTrail();
  assert.deepEqual(step(trail, "local", "back", always), { trail, key: null });
});

test("repeat and empty visits leave the trail alone, and it keeps only the newest picks", () => {
  const trail = emptyTrail();
  assert.equal(visit(trail, "local", "local"), trail);
  assert.equal(visit(trail, null, "local"), trail);
  let long = trail;
  for (let index = 0; index < TRAIL_LIMIT + 5; index += 1) long = visit(long, `k${index}`, "x");
  assert.equal(long.back.length, TRAIL_LIMIT);
  assert.equal(long.back.at(-1), `k${TRAIL_LIMIT + 4}`);
});

function burst(events, start = 0) {
  let swipe = null;
  const directions = [];
  events.forEach(([dx, dy], index) => {
    const result = trackSwipe(swipe, { dx, dy, at: start + index * 16 });
    swipe = result.swipe;
    if (result.direction) directions.push(result.direction);
  });
  return { swipe, directions };
}

test("a leftward swipe goes back once, however long its momentum runs", () => {
  const { directions } = burst(Array.from({ length: 40 }, () => [-20, 1]));
  assert.deepEqual(directions, ["back"]);
});

test("a rightward swipe goes forward", () => {
  assert.deepEqual(burst(Array.from({ length: 10 }, () => [30, 0])).directions, ["forward"]);
});

test("a short or vertical burst never steps", () => {
  const short = Math.floor((SWIPE_DISTANCE - 1) / 10);
  assert.deepEqual(burst(Array.from({ length: short }, () => [-10, 0])).directions, []);
  const vertical = [[0, -12], ...Array.from({ length: 20 }, () => [-30, -5])];
  assert.deepEqual(burst(vertical).directions, []);
});

test("a pause starts a new swipe", () => {
  let { swipe } = burst(Array.from({ length: 10 }, () => [-30, 0]));
  const later = swipe.at + SWIPE_QUIET_MS + 1;
  const directions = [];
  for (let index = 0; index < 10; index += 1) {
    const result = trackSwipe(swipe, { dx: -30, dy: 0, at: later + index * 16 });
    swipe = result.swipe;
    if (result.direction) directions.push(result.direction);
  }
  assert.deepEqual(directions, ["back"]);
});
