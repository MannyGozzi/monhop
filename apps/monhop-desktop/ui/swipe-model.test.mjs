import assert from "node:assert/strict";
import test from "node:test";

import { SWIPE_LABEL, SWIPE_NOTE, swipeContext } from "./swipe-model.mjs";

test("the switch shows only on a Mac once Rust has answered", () => {
  assert.equal(swipeContext("windows", { enabled: true, error: null }, false), undefined);
  assert.equal(swipeContext("other", { enabled: true, error: null }, false), undefined);
  assert.equal(swipeContext("macos", undefined, false), undefined);
  assert.deepEqual(swipeContext("macos", { enabled: false, error: null }, true), {
    view: { enabled: false, error: "" },
    pending: true,
  });
});

test("the switch is on unless Rust says off, and a save failure is shown", () => {
  assert.deepEqual(swipeContext("macos", null, false).view, { enabled: true, error: "" });
  assert.deepEqual(swipeContext("macos", { enabled: "no" }, false).view, {
    enabled: true,
    error: "",
  });
  assert.deepEqual(swipeContext("macos", { enabled: false, error: "not saved" }, false).view, {
    enabled: false,
    error: "not saved",
  });
  assert.equal(swipeContext("macos", { error: 3 }, false).view.error, "");
  assert.equal(swipeContext("macos", {}, "yes").pending, false);
});

test("the copy says what a swipe does, in plain words", () => {
  assert.equal(SWIPE_LABEL, "Swipe between pages");
  assert.match(SWIPE_NOTE, /two-finger swipe/);
  assert.match(SWIPE_NOTE, /back or forward on the other computer/);
});
