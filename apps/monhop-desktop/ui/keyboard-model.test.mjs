import assert from "node:assert/strict";
import test from "node:test";

import {
  CONTROL_AS_COMMAND_LABEL,
  CONTROL_AS_COMMAND_NOTE,
  controlAsCommandContext,
} from "./keyboard-model.mjs";

test("the switch shows only on Windows once Rust has answered", () => {
  assert.equal(controlAsCommandContext("macos", { enabled: true, error: null }, false), undefined);
  assert.equal(controlAsCommandContext("other", { enabled: true, error: null }, false), undefined);
  assert.equal(controlAsCommandContext("windows", undefined, false), undefined);
  assert.deepEqual(controlAsCommandContext("windows", { enabled: true, error: null }, true), {
    view: { enabled: true, error: "" },
    pending: true,
  });
});

test("the switch is off unless Rust says on, and a save failure is shown", () => {
  assert.deepEqual(controlAsCommandContext("windows", null, false).view, {
    enabled: false,
    error: "",
  });
  assert.equal(controlAsCommandContext("windows", { enabled: "yes" }, false).view.enabled, false);
  assert.deepEqual(
    controlAsCommandContext("windows", { enabled: false, error: "not saved" }, false).view,
    { enabled: false, error: "not saved" },
  );
  assert.equal(controlAsCommandContext("windows", {}, "yes").pending, false);
});

test("the copy names the shortcuts and the Windows key in plain words", () => {
  assert.equal(CONTROL_AS_COMMAND_LABEL, "Use Ctrl as Command on a Mac");
  assert.match(CONTROL_AS_COMMAND_NOTE, /Ctrl\+C, Ctrl\+V/);
  assert.match(CONTROL_AS_COMMAND_NOTE, /The Windows key becomes Control\./);
});
