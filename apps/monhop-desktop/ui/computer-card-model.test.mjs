import assert from "node:assert/strict";
import test from "node:test";

import {
  CONTROL_PAUSE_HINT,
  clearForgetFor,
  controlSwitchRows,
  displaysFreshness,
  isForgetArmed,
  keepForgetArmed,
  layoutChips,
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
    crossings: 1,
    layout: { links: [] },
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

test("one word under the picture says whether both computers are reporting their displays now", () => {
  const live = {
    localDisplays: [display("Built-in live")],
    peerDisplays: [display("LG live", { id: "2" })],
  };
  const setup = {
    localDisplays: [display("Built-in saved")],
    peerDisplays: [display("LG saved", { id: "2" })],
    live,
  };
  assert.equal(displaysFreshness(setup), "Live");
  // The link is up but has not reported the other computer's displays, so the picture is stale.
  assert.equal(displaysFreshness({ ...setup, live: { ...live, peerDisplays: [] } }), "Last seen");
  assert.equal(displaysFreshness({ ...setup, live: null }), "Last seen");
  // Nothing known is not "last seen": there is nothing to have seen.
  assert.equal(displaysFreshness(null), null);
  assert.equal(displaysFreshness({ localDisplays: [], peerDisplays: [], live: null }), null);
});

test("only an entry that says it was named by the user is marked Saved", () => {
  const fits = { tone: "connected", label: "Fits now" };
  assert.deepEqual(layoutChips(entry("Desk")), { marks: [], fits });
  assert.deepEqual(layoutChips(entry("Desk", { automatic: false })), {
    marks: [{ tone: "neutral", label: "Saved" }],
    fits,
  });
  // Rust always serializes the boolean, so an entry without it is damaged and claims nothing.
  assert.deepEqual(layoutChips(entry("Desk", { automatic: undefined })), { marks: [], fits });
  assert.deepEqual(layoutChips({}), { marks: [], fits: null });
  assert.deepEqual(layoutChips(entry("Desk", { fits: false })), { marks: [], fits: null });
});

test("no list draws a Remembered chip: every entry in a Layouts list is remembered", async () => {
  const { readFile } = await import("node:fs/promises");
  const files = ["computer-card-model.mjs", "computer-card.mjs", "screen-displays.mjs"];
  const sources = await Promise.all(
    files.map((file) => readFile(new URL(file, import.meta.url), "utf8")),
  );
  for (const [index, source] of sources.entries())
    assert.doesNotMatch(source, /"Remembered"/, files[index]);
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

test("renders_both_switches_on_by_default", () => {
  const rows = controlSwitchRows(null, "This Mac", "Office Windows PC", false);
  assert.equal(rows.length, 2);
  assert.deepEqual(
    rows.map((row) => [row.direction, row.checked, row.disabled, row.hint]),
    [
      ["localToPeer", true, false, ""],
      ["peerToLocal", true, false, ""],
    ],
  );
  assert.equal(rows[0].label, "This Mac can control Office Windows PC");
  assert.equal(rows[1].label, "Office Windows PC can control This Mac");
  // A record with both directions on reads exactly like no record at all.
  assert.deepEqual(
    controlSwitchRows({ localToPeer: true, peerToLocal: true }, "This Mac", "Office Windows PC", false),
    rows,
  );
});

test("last_enabled_switch_is_disabled_with_pause_hint", () => {
  const onlyLocalToPeer = controlSwitchRows(
    { localToPeer: true, peerToLocal: false },
    "This Mac",
    "Office Windows PC",
    false,
  );
  assert.equal(onlyLocalToPeer[0].checked, true);
  assert.equal(onlyLocalToPeer[0].disabled, true);
  assert.equal(onlyLocalToPeer[0].hint, CONTROL_PAUSE_HINT);
  // The other direction is off, so turning it back on is never blocked.
  assert.equal(onlyLocalToPeer[1].checked, false);
  assert.equal(onlyLocalToPeer[1].disabled, false);
  assert.equal(onlyLocalToPeer[1].hint, "");

  const onlyPeerToLocal = controlSwitchRows(
    { localToPeer: false, peerToLocal: true },
    "This Mac",
    "Office Windows PC",
    false,
  );
  assert.equal(onlyPeerToLocal[1].disabled, true);
  assert.equal(onlyPeerToLocal[1].hint, CONTROL_PAUSE_HINT);
  assert.equal(onlyPeerToLocal[0].disabled, false);
});

test("switches disable while the command is in flight or both computers are syncing", () => {
  const rows = controlSwitchRows(
    { localToPeer: true, peerToLocal: false },
    "This Mac",
    "Office Windows PC",
    true,
  );
  assert.deepEqual(
    rows.map((row) => row.disabled),
    [true, true],
  );
  const bothOn = controlSwitchRows(null, "This Mac", "Office Windows PC", true);
  assert.deepEqual(
    bothOn.map((row) => row.disabled),
    [true, true],
  );
  const backendSyncing = controlSwitchRows(
    { localToPeer: false, peerToLocal: true, syncing: true },
    "This Mac",
    "Office Windows PC",
    false,
  );
  assert.deepEqual(
    backendSyncing.map((row) => [row.disabled, row.hint]),
    [
      [true, "Updating both computers…"],
      [true, CONTROL_PAUSE_HINT],
    ],
  );
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
