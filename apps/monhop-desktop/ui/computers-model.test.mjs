import assert from "node:assert/strict";
import test from "node:test";

import {
  displayName,
  findComputer,
  initialComputers,
  normalizeComputers,
} from "./computers-model.mjs";

const WINDOWS = "b".repeat(64);
const MAC = "c".repeat(64);
const localId = "18446744073709551614";
const peerId = "18446744073709551615";

const edge = (from, fromEdge, to, toEdge) => ({
  fromDisplay: from,
  fromEdge,
  fromSpan: [0, 1],
  toDisplay: to,
  toEdge,
  toSpan: [0, 1],
  hysteresis: 1,
});

function savedLayout() {
  return {
    sourceDisplay: localId,
    links: [edge(localId, "right", peerId, "left"), edge(peerId, "left", localId, "right")],
  };
}

function setup() {
  return {
    saved: true,
    sourceSide: "local",
    localDisplays: [
      { id: localId, name: "Built-in", origin: [0, 0], size: [1512, 982], primary: true },
    ],
    peerDisplays: [{ id: peerId, name: "LG", origin: [0, 0], size: [2560, 1440], primary: true }],
    layout: savedLayout(),
    previewLayout: savedLayout(),
    message: "Saved layout matches this snapshot.",
  };
}

test("the computer list is bounded, deduplicated, and lowercased on the way in", () => {
  const view = normalizeComputers({
    computers: [
      { fingerprint: WINDOWS.toUpperCase(), name: "Office Windows PC", platform: "windows" },
      { fingerprint: WINDOWS, name: "Duplicate", platform: "windows" },
      { fingerprint: MAC, name: "Studio Mac", platform: "macos", address: "192.168.1.34:24872" },
      { fingerprint: "short", name: "Bad", platform: "windows" },
      { fingerprint: "d".repeat(64), name: "Linux box", platform: "linux" },
    ],
    active: WINDOWS.toUpperCase(),
    interfaceId: "en0:15:192.168.1.4",
  });
  assert.equal(view.loaded, true);
  assert.deepEqual(
    view.items.map((item) => item.fingerprint),
    [WINDOWS, MAC],
  );
  assert.equal(view.items[0].name, "Office Windows PC");
  assert.equal(view.items[1].address, "192.168.1.34:24872");
  assert.equal(view.items[0].address, null);
  // Fingerprints are compared with a plain === everywhere, so they arrive lowercase.
  assert.equal(view.active, WINDOWS);
  assert.equal(view.interfaceId, "en0:15:192.168.1.4");
});

test("a computer nobody is paired with is never the one in use", () => {
  const view = normalizeComputers({
    computers: [{ fingerprint: WINDOWS, name: "Office Windows PC", platform: "windows" }],
    active: MAC,
  });
  assert.equal(view.active, null);
  assert.equal(findComputer(view, MAC), null);
  assert.equal(findComputer(view, WINDOWS).name, "Office Windows PC");
  assert.equal(normalizeComputers({ computers: [], active: null }).active, null);
});

test("a malformed reply is never mistaken for an empty list", () => {
  for (const value of [null, undefined, [], "computers", {}, { computers: {} }])
    assert.deepEqual(normalizeComputers(value), initialComputers());
  assert.equal(initialComputers().loaded, false);
  assert.equal(normalizeComputers({ computers: [] }).loaded, true);
});

test("each computer carries its own saved layout, or an honest empty one", () => {
  const view = normalizeComputers({
    computers: [
      { fingerprint: WINDOWS, name: "Office Windows PC", platform: "windows", setup: setup() },
      { fingerprint: MAC, name: "Studio Mac", platform: "macos" },
    ],
    active: WINDOWS,
  });
  const saved = view.items[0].setup;
  assert.equal(saved.saved, true);
  assert.equal(saved.sourceSide, "local");
  assert.equal(saved.localDisplays.length, 1);
  assert.equal(saved.layout.sourceDisplay, localId);
  assert.equal(saved.previewLayout.links.length, 2);
  const blank = view.items[1].setup;
  assert.equal(blank.saved, false);
  assert.deepEqual(blank.localDisplays, []);
  assert.equal(blank.layout, null);
  // A layout the saved displays cannot realize is dropped rather than drawn wrong.
  const broken = normalizeComputers({
    computers: [
      {
        fingerprint: WINDOWS,
        name: "Office Windows PC",
        platform: "windows",
        setup: { ...setup(), layout: { sourceDisplay: "1e3", links: [] } },
      },
    ],
  });
  assert.equal(broken.items[0].setup.layout, null);
});

test("a computer without a name falls back to its platform", () => {
  assert.equal(displayName({ platform: "macos", name: "" }), "Mac");
  assert.equal(displayName({ platform: "windows", name: "" }), "Windows PC");
  assert.equal(displayName({ platform: "macos", name: "Studio Mac" }), "Studio Mac");
  assert.equal(displayName(null), "Windows PC");
  // Control and bidi characters never reach a label.
  const hostile = normalizeComputers({
    computers: [{ fingerprint: WINDOWS, name: "Office\u202ePC", platform: "windows" }],
  });
  assert.equal(displayName(hostile.items[0]), "Windows PC");
});
