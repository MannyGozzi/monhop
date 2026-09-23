import assert from "node:assert/strict";
import test from "node:test";

import { panelNeedsChange } from "./glide-model.mjs";

test("a settled panel changes only when asked for the other state", () => {
  assert.equal(panelNeedsChange({ hidden: true, open: false }), false);
  assert.equal(panelNeedsChange({ hidden: false, open: true }), false);
  assert.equal(panelNeedsChange({ hidden: true, open: true }), true);
  assert.equal(panelNeedsChange({ hidden: false, open: false }), true);
});

test("a re-render that repeats a gliding panel's target leaves the glide running", () => {
  // Mid-close the panel is not hidden yet, so `hidden` alone would restart the close; the target decides.
  assert.equal(panelNeedsChange({ glidingTo: true, hidden: false, open: true }), false);
  assert.equal(panelNeedsChange({ glidingTo: false, hidden: false, open: false }), false);
});

test("the other target reverses a glide, and an instant request lands it", () => {
  assert.equal(panelNeedsChange({ glidingTo: true, hidden: false, open: false }), true);
  assert.equal(panelNeedsChange({ glidingTo: false, hidden: false, open: true }), true);
  assert.equal(
    panelNeedsChange({ glidingTo: true, hidden: false, open: true, instant: true }),
    true,
  );
});
