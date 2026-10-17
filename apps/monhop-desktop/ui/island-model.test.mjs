import assert from "node:assert/strict";
import test from "node:test";

import {
  ISLAND_DESCRIPTION,
  ISLAND_SWITCH_LABEL,
  ISLAND_TITLE,
  islandContext,
  normalizeIslandView,
} from "./island-model.mjs";

test("the card shows only on a Mac with a notched display", () => {
  const view = { supported: true, enabled: false, error: null };
  assert.equal(islandContext("windows", view, false), undefined);
  assert.equal(islandContext("other", view, false), undefined);
  assert.equal(islandContext("macos", undefined, false), undefined);
  assert.equal(islandContext("macos", { ...view, supported: false }, false), undefined);
  assert.equal(islandContext("macos", null, false), undefined);
  assert.deepEqual(islandContext("macos", view, true), {
    view: { supported: true, enabled: false, error: "" },
    pending: true,
  });
});

test("the switch is off unless Rust says on, and a save failure is shown", () => {
  assert.deepEqual(normalizeIslandView({ supported: true }), {
    supported: true,
    enabled: false,
    error: "",
  });
  assert.equal(normalizeIslandView({ supported: true, enabled: "yes" }).enabled, false);
  assert.equal(normalizeIslandView({ supported: true, error: 3 }).error, "");
  assert.equal(normalizeIslandView({ supported: true, error: "not saved" }).error, "not saved");
  assert.equal(islandContext("macos", { supported: true }, "yes").pending, false);
});

test("the copy points at the notch and names every control", () => {
  assert.equal(ISLAND_TITLE, "Notch island");
  assert.match(ISLAND_SWITCH_LABEL, /notch/);
  assert.match(ISLAND_DESCRIPTION, /Point at the notch/);
  assert.match(ISLAND_DESCRIPTION, /sharing, dimming and clipboard/);
  assert.doesNotMatch(ISLAND_DESCRIPTION, /menu bar/);
});
