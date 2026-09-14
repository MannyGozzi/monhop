import assert from "node:assert/strict";
import test from "node:test";

import { setupSectionGates, shouldShowPairing } from "./setup-presentation.mjs";

test("a section stays shut until the one before it is done, and says so in its own header", () => {
  const nothing = setupSectionGates({ readyDone: false, pairedCount: 0, displaysReady: false });
  assert.equal(nothing.ready.locked, false);
  assert.equal(nothing.computers.locked, true);
  assert.equal(nothing.computers.reason, "Finish Get ready first");
  assert.equal(nothing.displays.locked, true);
  assert.equal(nothing.displays.reason, "Finish Get ready first");

  const ready = setupSectionGates({ readyDone: true, pairedCount: 0, displaysReady: false });
  assert.equal(ready.ready.done, true);
  assert.equal(ready.computers.locked, false);
  assert.equal(ready.computers.done, false);
  assert.equal(ready.displays.reason, "Pair a computer first");
});

test("Displays waits for the computer in use to answer, and names which one", () => {
  const paused = setupSectionGates({ readyDone: true, pairedCount: 2, displaysReady: false });
  assert.equal(paused.computers.done, true);
  assert.equal(paused.displays.reason, "Choose a computer to use");

  const dialing = setupSectionGates({
    readyDone: true,
    pairedCount: 1,
    activeName: "Noctua Windows PC",
    displaysReady: false,
  });
  assert.equal(dialing.displays.locked, true);
  assert.equal(dialing.displays.reason, "Waiting for Noctua Windows PC");

  const live = setupSectionGates({
    readyDone: true,
    pairedCount: 1,
    activeName: "Noctua Windows PC",
    displaysReady: true,
    layoutSaved: true,
  });
  assert.equal(live.displays.locked, false);
  assert.equal(live.displays.reason, "");
  assert.equal(live.displays.done, true);
});

test("a locked section is never marked done, however far the rest of setup got", () => {
  const gates = setupSectionGates({
    readyDone: false,
    pairedCount: 3,
    activeName: "Studio Mac",
    displaysReady: true,
    layoutSaved: true,
  });
  assert.equal(gates.computers.done, false);
  assert.equal(gates.displays.done, false);
  assert.deepEqual([gates.ready.index, gates.computers.index, gates.displays.index], [0, 1, 2]);
  assert.deepEqual(
    [gates.ready.title, gates.computers.title, gates.displays.title],
    ["Get ready", "Computers", "Displays"],
  );
});

test("the code exchange opens on a fresh install, on request, and while an exchange is running", () => {
  assert.equal(shouldShowPairing({ pairedCount: 0 }), true);
  assert.equal(shouldShowPairing({ pairedCount: 1 }), false);
  assert.equal(shouldShowPairing({ pairedCount: 1, requested: true }), true);
  assert.equal(shouldShowPairing({ pairedCount: 1, phase: "ready" }), false);
  assert.equal(shouldShowPairing({ pairedCount: 1, phase: "paired" }), false);
  for (const phase of ["identity-missing", "review", "waiting", "saving", "error"])
    assert.equal(shouldShowPairing({ pairedCount: 2, phase }), true, phase);
  assert.equal(shouldShowPairing(), true);
});
