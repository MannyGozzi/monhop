import assert from "node:assert/strict";
import test from "node:test";

import {
  applyDimmingView,
  beginDimming,
  clampLevel,
  dimButtonLabel,
  dimmingMessage,
  failDimming,
  initialDimming,
  levelLabel,
  shortcutDescription,
} from "./dimming-model.mjs";

const view = {
  enabled: true,
  level: 50,
  minLevel: 10,
  maxLevel: 99,
  dimmed: false,
  shortcut: "Control + Option + 0",
  error: null,
};

test("a view from Rust settles a pending request and clears the failure", () => {
  const pending = beginDimming(failDimming(initialDimming(), "boom"));
  assert.equal(pending.pending, true);
  assert.equal(pending.failure, "");
  const settled = applyDimmingView(pending, view);
  assert.deepEqual(settled, { view, pending: false, failure: "" });
});

test("a failed request keeps the last view so the controls stay usable", () => {
  const failed = failDimming(applyDimmingView(initialDimming(), view), "no");
  assert.equal(failed.view, view);
  assert.equal(failed.pending, false);
  assert.equal(dimmingMessage(failed), "no");
  assert.equal(dimmingMessage(applyDimmingView(failed, { ...view, error: "late" })), "late");
  assert.equal(dimmingMessage(initialDimming()), "");
});

test("labels follow the overlay and the shortcut state", () => {
  assert.equal(dimButtonLabel(view), "Dim now");
  assert.equal(dimButtonLabel({ ...view, dimmed: true }), "Undim");
  assert.equal(dimButtonLabel(null), "Dim now");
  assert.equal(levelLabel(35), "35%");
  assert.match(shortcutDescription(view), /^Control \+ Option \+ 0 toggles/);
  assert.match(shortcutDescription({ ...view, enabled: false }), /^Off\./);
  assert.equal(shortcutDescription(null), "Toggles the dimming from any app.");
});

test("a level keeps whole percents inside the bounds Rust reports", () => {
  assert.equal(clampLevel(view, 52), 52);
  assert.equal(clampLevel(view, 53.4), 53);
  assert.equal(clampLevel(view, 3), 10);
  assert.equal(clampLevel(view, 120), 99);
  assert.equal(clampLevel(null, 47), 47);
});
