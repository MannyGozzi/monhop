import assert from "node:assert/strict";
import test from "node:test";

import {
  COMMAND_AS_CONTROL_LABEL,
  COMMAND_AS_CONTROL_NOTE,
  CONTROL_AS_COMMAND_LABEL,
  CONTROL_AS_COMMAND_NOTE,
  keyboardCommands,
  keyboardContext,
} from "./keyboard-model.mjs";

test("the page shows on a Mac and a PC once Rust has answered", () => {
  assert.equal(keyboardContext("other", { enabled: true, error: null }, false), undefined);
  assert.equal(keyboardContext("windows", undefined, false), undefined);
  assert.equal(keyboardContext("macos", undefined, false), undefined);
  for (const platform of ["windows", "macos"]) {
    const context = keyboardContext(platform, { enabled: true, error: null }, true);
    assert.deepEqual(context.view, { enabled: true, error: "" });
    assert.equal(context.pending, true);
  }
});

test("each computer's switch is the one for its own keyboard", () => {
  const windows = keyboardContext("windows", {}, false);
  assert.equal(windows.label, CONTROL_AS_COMMAND_LABEL);
  assert.equal(windows.note, CONTROL_AS_COMMAND_NOTE);
  assert.deepEqual(keyboardCommands("windows"), {
    status: "control_as_command_status",
    set: "control_as_command_set_enabled",
  });
  const mac = keyboardContext("macos", {}, false);
  assert.equal(mac.label, COMMAND_AS_CONTROL_LABEL);
  assert.equal(mac.note, COMMAND_AS_CONTROL_NOTE);
  assert.deepEqual(keyboardCommands("macos"), {
    status: "command_as_control_status",
    set: "command_as_control_set_enabled",
  });
  assert.equal(keyboardCommands("other"), undefined);
});

test("beside it, the other platform's switch names the computer that changes it", () => {
  const windows = keyboardContext("windows", {}, false).other;
  assert.equal(windows.title, "Command as Ctrl on Windows");
  assert.match(windows.detail, /Mac keyboard that controls this PC/);
  assert.match(windows.detail, /in MonHop on that Mac, under Settings › Keyboard\./);
  const mac = keyboardContext("macos", {}, false).other;
  assert.equal(mac.title, "Ctrl as Command on a Mac");
  assert.match(mac.detail, /Windows keyboard that controls this Mac/);
  assert.match(mac.detail, /in MonHop on that PC, under Settings › Keyboard\./);
});

test("the switch is off unless Rust says on, and a save failure is shown", () => {
  for (const platform of ["windows", "macos"]) {
    assert.deepEqual(keyboardContext(platform, null, false).view, { enabled: false, error: "" });
    assert.equal(keyboardContext(platform, { enabled: "yes" }, false).view.enabled, false);
    assert.deepEqual(
      keyboardContext(platform, { enabled: false, error: "not saved" }, false).view,
      { enabled: false, error: "not saved" },
    );
    assert.equal(keyboardContext(platform, {}, "yes").pending, false);
  }
});

test("the copy names the shortcuts and the swapped keys in plain words", () => {
  assert.equal(CONTROL_AS_COMMAND_LABEL, "Use Ctrl as Command on a Mac");
  assert.match(CONTROL_AS_COMMAND_NOTE, /Ctrl\+C, Ctrl\+V/);
  assert.match(CONTROL_AS_COMMAND_NOTE, /The Windows key becomes Control\./);
  assert.equal(COMMAND_AS_CONTROL_LABEL, "Use Command as Ctrl on Windows");
  assert.match(COMMAND_AS_CONTROL_NOTE, /Command\+C, Command\+V/);
  assert.match(COMMAND_AS_CONTROL_NOTE, /Control becomes the Windows key\./);
});
