import assert from "node:assert/strict";
import test from "node:test";

import {
  clearForgetFor,
  displayChipLabel,
  displayStripSides,
  hasDisplayStrip,
  isForgetArmed,
  keepForgetArmed,
  layoutRowKey,
  layoutRows,
  loadGate,
  newestFirst,
  pressForget,
} from "./computer-card-model.mjs";

const WINDOWS = "b".repeat(64);
const MAC = "c".repeat(64);

function entry(name, patch = {}) {
  return {
    name,
    sourceSide: "local",
    mode: "grouped",
    crossings: 1,
    layout: { sourceDisplay: "1", links: [] },
    automatic: true,
    fits: true,
    ...patch,
  };
}

function display(name, patch = {}) {
  return { id: "1", name, origin: [0, 0], size: [1512, 982], primary: true, ...patch };
}

test("every row of one computer gets its own focus key, whatever the names are", () => {
  const keys = ["Desk", "Desk 2", "Couch", "x".repeat(64), "Ｄesk"].map((name) =>
    layoutRowKey(WINDOWS, name),
  );
  assert.equal(new Set(keys).size, keys.length);
  for (const key of keys) assert.ok(key.length < 80, key);
  // The same name under another computer is a different row.
  assert.notEqual(layoutRowKey(WINDOWS, "Desk"), layoutRowKey(MAC, "Desk"));
  // The key is stable, so focus survives a re-render.
  assert.equal(layoutRowKey(WINDOWS, "Desk"), layoutRowKey(WINDOWS, "Desk"));
});

test("the newest layout leads, and a malformed list is simply empty", () => {
  assert.deepEqual(
    newestFirst([entry("first"), entry("second"), entry("third")]).map((item) => item.name),
    ["third", "second", "first"],
  );
  assert.deepEqual(newestFirst(null), []);
  const original = [entry("first"), entry("second")];
  newestFirst(original);
  assert.deepEqual(
    original.map((item) => item.name),
    ["first", "second"],
  );
});

test("each side of the display strip falls back on its own, and says when it did", () => {
  const setup = {
    localDisplays: [display("Built-in saved")],
    peerDisplays: [display("LG saved", { id: "2" })],
    live: { localDisplays: [display("Built-in live")], peerDisplays: [] },
  };
  const sides = displayStripSides(setup);
  assert.deepEqual(
    sides.local.displays.map((item) => item.name),
    ["Built-in live"],
  );
  assert.equal(sides.local.lastSeen, false);
  // The link is up but has not reported the other computer's displays: the saved ones, marked.
  assert.deepEqual(
    sides.peer.displays.map((item) => item.name),
    ["LG saved"],
  );
  assert.equal(sides.peer.lastSeen, true);
  assert.equal(hasDisplayStrip(sides), true);

  const savedOnly = displayStripSides({ ...setup, live: null });
  assert.equal(savedOnly.local.lastSeen, true);
  assert.equal(savedOnly.peer.lastSeen, true);

  const nothing = displayStripSides(null);
  assert.equal(hasDisplayStrip(nothing), false);
  // Nothing known is not "last seen": there is nothing to have seen.
  assert.equal(nothing.local.lastSeen, false);
});

test("a display chip reads in native pixels when they are known, logical otherwise", () => {
  assert.equal(
    displayChipLabel(display("Built-in", { nativeSize: [3024, 1964], scale: 2 })),
    "Built-in · 3024 × 1964",
  );
  assert.equal(displayChipLabel(display("Built-in")), "Built-in · 1512 × 982");
  assert.equal(
    displayChipLabel(display("Built-in", { nativeSize: [3024, Number.NaN] })),
    "Built-in · 1512 × 982",
  );
  assert.equal(displayChipLabel({ name: "", size: null }), "Display");
});

test("Load is enabled only when pressing it would really load, and says why when it is not", () => {
  const connectedHere = { isActive: true, connected: true, entry: entry("Desk") };
  assert.deepEqual(loadGate(connectedHere), { enabled: true, reason: "" });

  const elsewhere = loadGate({ ...connectedHere, isActive: false });
  assert.equal(elsewhere.enabled, false);
  assert.match(elsewhere.reason, /use this computer/i);

  const offline = loadGate({ ...connectedHere, connected: false });
  assert.equal(offline.enabled, false);
  assert.match(offline.reason, /connected/i);

  for (const patch of [{ fits: false }, { layout: null }]) {
    const misfit = loadGate({ ...connectedHere, entry: entry("Desk", patch) });
    assert.equal(misfit.enabled, false, JSON.stringify(patch));
    assert.match(misfit.reason, /does not fit/i);
  }
  // An entry the list no longer holds is never loadable.
  assert.equal(loadGate({ isActive: true, connected: true, entry: null }).enabled, false);
});

test("Forget takes two presses, on one row across the whole app", () => {
  let armed = null;
  const first = pressForget(armed, WINDOWS, "Desk");
  assert.equal(first.forget, false);
  armed = first.armed;
  assert.equal(isForgetArmed(armed, WINDOWS, "Desk"), true);
  // The same name under another computer, and another name here, are both still unarmed.
  assert.equal(isForgetArmed(armed, MAC, "Desk"), false);
  assert.equal(isForgetArmed(armed, WINDOWS, "Couch"), false);

  const other = pressForget(armed, WINDOWS, "Couch");
  assert.equal(other.forget, false);
  assert.equal(isForgetArmed(other.armed, WINDOWS, "Couch"), true);
  assert.equal(isForgetArmed(other.armed, WINDOWS, "Desk"), false);

  const second = pressForget(armed, WINDOWS, "Desk");
  assert.equal(second.forget, true);
  assert.equal(second.armed, null);
});

test("an armed row is dropped when its list is read again or its computer is unpaired", () => {
  const armed = { fingerprint: WINDOWS, name: "Desk" };
  assert.equal(clearForgetFor(armed, WINDOWS), null);
  assert.deepEqual(clearForgetFor(armed, MAC), armed);
  assert.deepEqual(keepForgetArmed(armed, [WINDOWS, MAC]), armed);
  assert.equal(keepForgetArmed(armed, [MAC]), null);
  assert.equal(keepForgetArmed(null, [WINDOWS]), null);
});

test("a computer's rows are ordered, keyed, gated and locked in one pass", () => {
  const entries = [entry("Desk"), entry("Couch", { automatic: false, fits: false, layout: null })];
  const built = layoutRows({
    fingerprint: WINDOWS,
    entries,
    armed: { fingerprint: WINDOWS, name: "Desk" },
    isActive: true,
    connected: true,
    busy: false,
    pending: false,
  });
  assert.deepEqual(
    built.map((item) => item.entry.name),
    ["Couch", "Desk"],
  );
  assert.equal(built[1].armed, true);
  assert.equal(built[0].armed, false);
  assert.equal(built[1].load.enabled, true);
  assert.equal(built[0].load.enabled, false);
  assert.equal(new Set(built.map((item) => item.key)).size, 2);
  assert.deepEqual(
    built.map((item) => item.disabled),
    [false, false],
  );

  // This computer's own read or forget in flight locks its rows, and so does an app-wide command.
  for (const patch of [{ pending: true }, { busy: true }])
    assert.deepEqual(
      layoutRows({
        fingerprint: WINDOWS,
        entries,
        armed: null,
        isActive: true,
        connected: true,
        busy: false,
        pending: false,
        ...patch,
      }).map((item) => item.disabled),
      [true, true],
      JSON.stringify(patch),
    );
});
