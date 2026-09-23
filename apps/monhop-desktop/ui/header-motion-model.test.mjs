import assert from "node:assert/strict";
import test from "node:test";

import { slotChange, slotPop } from "./header-motion-model.mjs";

test("a render that repeats the slot's target leaves any running pop alone", () => {
  assert.equal(slotChange({ shown: true, present: true, motion: true }), null);
  assert.equal(slotChange({ shown: false, present: false, motion: true }), null);
  assert.equal(slotChange({ shown: false, present: false, motion: false }), null);
});

test("the other target animates, unless it is the first render or motion is off", () => {
  assert.deepEqual(slotChange({ shown: false, present: true, motion: true }), { animate: true });
  assert.deepEqual(slotChange({ shown: true, present: false, motion: true }), { animate: true });
  assert.deepEqual(slotChange({ shown: null, present: true, motion: true }), { animate: false });
  assert.deepEqual(slotChange({ shown: null, present: false, motion: true }), { animate: false });
  assert.deepEqual(slotChange({ shown: true, present: false, motion: false }), { animate: false });
});

test("a settled slot pops between rest and gone, entering a beat late", () => {
  assert.deepEqual(slotPop({ present: true, from: null, goneScale: 0.8 }), {
    from: { opacity: 0, scale: 0.8 },
    to: { opacity: 1, scale: 1 },
    wait: true,
  });
  assert.deepEqual(slotPop({ present: false, from: null, goneScale: 0.8 }), {
    from: { opacity: 1, scale: 1 },
    to: { opacity: 0, scale: 0.8 },
    wait: false,
  });
});

test("a reversal starts from the look on screen and does not wait", () => {
  const midway = { opacity: 0.4, scale: 0.9 };
  assert.deepEqual(slotPop({ present: true, from: midway, goneScale: 0.8 }), {
    from: midway,
    to: { opacity: 1, scale: 1 },
    wait: false,
  });
  assert.deepEqual(slotPop({ present: false, from: midway, goneScale: 0.8 }), {
    from: midway,
    to: { opacity: 0, scale: 0.8 },
    wait: false,
  });
});

test("a slot called back after it already faded out waits its beat again", () => {
  const faded = { opacity: 0, scale: 0.85 };
  assert.equal(slotPop({ present: true, from: faded, goneScale: 0.8 }).wait, true);
});
