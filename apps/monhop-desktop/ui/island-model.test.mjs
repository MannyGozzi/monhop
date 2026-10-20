import assert from "node:assert/strict";
import test from "node:test";

import {
  ISLAND_CLIPBOARD_DESCRIPTION,
  ISLAND_CLIPBOARD_LABEL,
  ISLAND_DESCRIPTION,
  ISLAND_SWITCH_LABEL,
  ISLAND_TITLE,
  islandContext,
  normalizeIslandView,
} from "./island-model.mjs";

test("the card shows only on a Mac with a notched display", () => {
  const view = { supported: true, enabled: false, clipboardNotices: false, error: null };
  assert.equal(islandContext("windows", view, false), undefined);
  assert.equal(islandContext("other", view, false), undefined);
  assert.equal(islandContext("macos", undefined, false), undefined);
  assert.equal(islandContext("macos", { ...view, supported: false }, false), undefined);
  assert.equal(islandContext("macos", null, false), undefined);
  assert.deepEqual(islandContext("macos", view, true), {
    view: { supported: true, enabled: false, clipboardNotices: false, error: "" },
    pending: true,
  });
});

test("both switches are off unless Rust says on, and a save failure is shown", () => {
  assert.deepEqual(normalizeIslandView({ supported: true }), {
    supported: true,
    enabled: false,
    clipboardNotices: false,
    error: "",
  });
  assert.equal(normalizeIslandView({ supported: true, enabled: "yes" }).enabled, false);
  assert.equal(
    normalizeIslandView({ supported: true, clipboardNotices: 1 }).clipboardNotices,
    false,
  );
  assert.equal(
    normalizeIslandView({ supported: true, clipboardNotices: true }).clipboardNotices,
    true,
  );
  assert.equal(normalizeIslandView({ supported: true, error: 3 }).error, "");
  assert.equal(normalizeIslandView({ supported: true, error: "not saved" }).error, "not saved");
  assert.equal(islandContext("macos", { supported: true }, "yes").pending, false);
});

test("the copy describes status notes beside the notch, with no hover controls", () => {
  assert.equal(ISLAND_TITLE, "Notch island");
  assert.match(ISLAND_SWITCH_LABEL, /notch/);
  assert.match(ISLAND_DESCRIPTION, /beside the notch/);
  assert.match(ISLAND_DESCRIPTION, /connecting, connects or drops/);
  assert.doesNotMatch(ISLAND_DESCRIPTION, /Point at|controls|dimming|menu bar/);
  assert.equal(ISLAND_CLIPBOARD_LABEL, "Show clipboard copies");
  assert.match(ISLAND_CLIPBOARD_DESCRIPTION, /^A short note beside the notch/);
  assert.match(ISLAND_CLIPBOARD_DESCRIPTION, /another computer\.$/);
});
