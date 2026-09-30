import assert from "node:assert/strict";
import test from "node:test";

import {
  AUTOSCROLL_LABEL,
  AUTOSCROLL_NOTE,
  autoscrollContext,
  normalizeAutoscrollView,
} from "./autoscroll-model.mjs";

test("the switch shows only on a Mac once Rust has answered", () => {
  assert.equal(autoscrollContext("windows", { enabled: true, error: null }, false), undefined);
  assert.equal(autoscrollContext("other", { enabled: true, error: null }, false), undefined);
  assert.equal(autoscrollContext("macos", undefined, false), undefined);
  assert.deepEqual(autoscrollContext("macos", { enabled: false, error: null }, true), {
    view: { enabled: false, error: "" },
    pending: true,
  });
});

test("the switch is on unless Rust says off, and a save failure is shown", () => {
  assert.deepEqual(normalizeAutoscrollView(null), { enabled: true, error: "" });
  assert.deepEqual(normalizeAutoscrollView({ enabled: "no" }), { enabled: true, error: "" });
  assert.deepEqual(normalizeAutoscrollView({ enabled: false, error: "not saved" }), {
    enabled: false,
    error: "not saved",
  });
  assert.equal(normalizeAutoscrollView({ error: 3 }).error, "");
});

test("the copy stays short and names the Windows mouse", () => {
  assert.equal(AUTOSCROLL_LABEL, "Middle-click autoscroll");
  assert.match(AUTOSCROLL_NOTE, /Windows mouse/);
});
