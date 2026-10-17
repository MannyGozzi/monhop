import assert from "node:assert/strict";
import test from "node:test";

import {
  ISLAND_TITLE,
  islandContext,
  islandDescription,
  islandSwitchLabel,
  normalizeIslandView,
} from "./island-model.mjs";

test("the card shows only on a Mac whose backend can draw the island", () => {
  const view = { supported: true, enabled: false, notched: true, error: null };
  assert.equal(islandContext("windows", view, false), undefined);
  assert.equal(islandContext("other", view, false), undefined);
  assert.equal(islandContext("macos", undefined, false), undefined);
  assert.equal(islandContext("macos", { ...view, supported: false }, false), undefined);
  assert.equal(islandContext("macos", null, false), undefined);
  assert.deepEqual(islandContext("macos", view, true), {
    view: { supported: true, enabled: false, notched: true, error: "" },
    pending: true,
  });
});

test("the switch is off unless Rust says on, and a save failure is shown", () => {
  assert.deepEqual(normalizeIslandView({ supported: true }), {
    supported: true,
    enabled: false,
    notched: false,
    error: "",
  });
  assert.equal(normalizeIslandView({ supported: true, enabled: "yes" }).enabled, false);
  assert.equal(normalizeIslandView({ supported: true, error: 3 }).error, "");
  assert.equal(normalizeIslandView({ supported: true, error: "not saved" }).error, "not saved");
  assert.equal(islandContext("macos", { supported: true }, "yes").pending, false);
});

test("the copy names the notch only on a display that has one", () => {
  assert.equal(ISLAND_TITLE, "Notch island");
  assert.match(islandSwitchLabel(true), /notch/);
  assert.doesNotMatch(islandSwitchLabel(false), /notch/);
  assert.match(islandDescription(true), /Point at the notch/);
  assert.match(islandDescription(false), /menu bar/);
  for (const notched of [true, false])
    assert.match(islandDescription(notched), /sharing, dimming and clipboard/);
});
