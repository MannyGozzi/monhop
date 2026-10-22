import assert from "node:assert/strict";
import test from "node:test";

import {
  LIVE_POINTER_LABEL,
  LIVE_POINTER_NOTE,
  livePointerContext,
} from "./live-pointer-model.mjs";

test("the switch shows on a Mac and on Windows once Rust has answered", () => {
  assert.equal(livePointerContext("other", { enabled: true, error: null }, false), undefined);
  assert.equal(livePointerContext("macos", undefined, false), undefined);
  assert.equal(livePointerContext("windows", undefined, false), undefined);
  for (const platform of ["macos", "windows"]) {
    assert.deepEqual(livePointerContext(platform, { enabled: false, error: null }, true), {
      view: { enabled: false, error: "" },
      pending: true,
    });
  }
});

test("the switch is on unless Rust says off", () => {
  assert.deepEqual(livePointerContext("windows", null, false).view, { enabled: true, error: "" });
  assert.deepEqual(livePointerContext("macos", { enabled: "no" }, false).view, {
    enabled: true,
    error: "",
  });
  assert.equal(livePointerContext("macos", {}, "yes").pending, false);
});

test("a save failure is shown", () => {
  assert.deepEqual(
    livePointerContext("windows", { enabled: false, error: "not saved" }, false).view,
    { enabled: false, error: "not saved" },
  );
  assert.equal(livePointerContext("macos", { error: 3 }, false).view.error, "");
});

test("the copy says what the marks do, in plain words", () => {
  assert.equal(LIVE_POINTER_LABEL, "Show which pointer is live");
  assert.match(LIVE_POINTER_NOTE, /Dims this computer's pointer/);
  assert.match(LIVE_POINTER_NOTE, /rings the pointer where control lands/);
});
