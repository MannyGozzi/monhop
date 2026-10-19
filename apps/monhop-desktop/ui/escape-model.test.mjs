import test from "node:test";
import assert from "node:assert/strict";
import { ESCAPE_SENTENCE, escapeKeys } from "./escape-model.mjs";

test("each keyboard shows the emergency stop in its own labels", () => {
  assert.deepEqual(escapeKeys("macos"), ["control", "control", "esc"]);
  assert.deepEqual(escapeKeys("windows"), ["Ctrl", "Ctrl", "Esc"]);
  assert.match(ESCAPE_SENTENCE, /both Control keys and Esc for 2 seconds/);
});
