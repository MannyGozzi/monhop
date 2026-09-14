import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import {
  applyTrialView,
  applyTrialStartResult,
  beginTrialClose,
  beginTrialStart,
  beginTrialStop,
  canNavigateTrialControls,
  canReturnToSetup,
  canStartTrial,
  canStopTrial,
  closeFailed,
  initialTrialState,
  normalizeTrialView,
  shouldPollTrial,
} from "./trial-model.mjs";

const active = {
  phase: "active",
  message: "Both windows are receiving the controlled test.",
  remainingSeconds: 42,
  sentEvents: "12",
  receivedEvents: "11",
  nativeArrivals: { keyboard: 2, button: 2, movement: 1, scroll: 1 },
  busy: false,
  retryableStart: false,
  preflightFailure: false,
};

test("opening a trial starts prepared and inert", () => {
  const state = initialTrialState(true);
  assert.equal(state.phase, "prepared");
  assert.equal(canStartTrial(state), true);
  assert.equal(canStopTrial(state), true);
  assert.equal(shouldPollTrial(state), false);
});

test("Start is single-use and enables trial-only polling", () => {
  let state = beginTrialStart(initialTrialState(true));
  assert.equal(state.phase, "starting");
  assert.equal(state.startConsumed, true);
  assert.equal(canStartTrial(state), false);
  assert.equal(shouldPollTrial(state), false);
  state = applyTrialStartResult(state, active);
  assert.equal(state.phase, "active");
  assert.equal(state.sentEvents, "12");
  assert.equal(state.receivedEvents, "11");
  assert.equal(shouldPollTrial(state), true);
});

test("a stale prepared receipt cannot reopen Start before the command settles", () => {
  const pending = beginTrialStart(initialTrialState(true));
  const stale = applyTrialView(pending, {
    ...active,
    phase: "prepared",
    message: "Start here and on the other computer.",
    retryableStart: true,
    preflightFailure: false,
  });
  assert.equal(stale.startRequestPending, true);
  assert.equal(stale.phase, "starting");
  assert.equal(stale.retryableStart, false);
  assert.equal(canStartTrial(stale), false);
  assert.equal(canNavigateTrialControls(stale), false);
  assert.equal(shouldPollTrial(stale), false);

  const rejected = applyTrialStartResult(stale, {
    ...active,
    phase: "prepared",
    message: "The test did not start: focus the test window.",
    retryableStart: true,
    preflightFailure: true,
  });
  assert.equal(rejected.startRequestPending, false);
  assert.equal(canStartTrial(rejected), true);
});

test("Stop remains available until a terminal receipt", () => {
  let state = beginTrialStop(
    applyTrialStartResult(beginTrialStart(initialTrialState(true)), active),
  );
  assert.equal(state.phase, "stopping");
  assert.equal(canStopTrial(state), true);
  state = applyTrialView(state, { ...active, phase: "finished", remainingSeconds: 0, busy: false });
  assert.equal(canStopTrial(state), false);
  assert.equal(canStartTrial(state), false);
  assert.equal(shouldPollTrial(state), false);
});

test("bad start status locks Start rather than re-enabling it", () => {
  const state = applyTrialStartResult(beginTrialStart(initialTrialState(true)), {
    ...active,
    sentEvents: "-1",
  });
  assert.equal(state.phase, "error");
  assert.equal(state.statusLost, true);
  assert.equal(canStartTrial(state), false);
  assert.equal(canStopTrial(state), true);
  assert.equal(shouldPollTrial(state), false);
});

test("only the current native start result restores Start after preflight", () => {
  const rejected = applyTrialStartResult(beginTrialStart(initialTrialState(true)), {
    ...active,
    phase: "prepared",
    message: "The test did not start: focus the test window.",
    retryableStart: true,
    preflightFailure: true,
  });
  assert.equal(rejected.preflightFailure, true);
  assert.equal(rejected.startConsumed, false);
  assert.equal(rejected.statusLost, false);
  assert.equal(canStartTrial(rejected), true);
  assert.equal(canNavigateTrialControls(rejected), true);

  const retry = beginTrialStart(rejected);
  assert.equal(retry.preflightFailure, false);
  assert.equal(retry.retryableStart, false);
  assert.equal(canStartTrial(retry), false);
  assert.equal(normalizeTrialView({ ...active, retryableStart: true, phase: "error" }), null);
  assert.equal(normalizeTrialView({ ...active, preflightFailure: true }), null);
});

test("input counters remain exact strings rather than JavaScript numbers", () => {
  const maxU64 = "18446744073709551615";
  const view = normalizeTrialView({ ...active, sentEvents: maxU64, receivedEvents: "0" });
  assert.equal(view.sentEvents, maxU64);
  assert.equal(view.receivedEvents, "0");
  assert.equal(normalizeTrialView({ ...active, sentEvents: 12 }), null);
});

test("a valid terminal error cannot be restarted or stopped", () => {
  const state = applyTrialStartResult(beginTrialStart(initialTrialState(true)), {
    ...active,
    phase: "error",
    message: "The native test ended.",
  });
  assert.equal(state.statusLost, false);
  assert.equal(canStartTrial(state), false);
  assert.equal(canStopTrial(state), false);
});

test("malformed status leaves Stop available until native cleanup is confirmed", () => {
  let state = applyTrialStartResult(beginTrialStart(initialTrialState(true)), {
    ...active,
    remainingSeconds: 61,
  });
  assert.equal(state.statusLost, true);
  state = beginTrialStop(state);
  assert.equal(state.phase, "stopping");
  assert.equal(state.busy, true);
  assert.equal(canStartTrial(state), false);
});

test("only native category counts are accepted as arrival evidence", () => {
  for (const value of [
    undefined,
    null,
    {},
    { keyboard: -1, button: 0, movement: 0, scroll: 0 },
    { keyboard: "2", button: 0, movement: 0, scroll: 0 },
  ]) {
    assert.equal(normalizeTrialView({ ...active, nativeArrivals: value }), null);
  }
  assert.deepEqual(normalizeTrialView(active).nativeArrivals, active.nativeArrivals);
});

test("startup diagnostics survive the native view without suggesting input was sent", () => {
  const message = "Both computers did not become ready in time. [Startup: Deadline]";
  const state = applyTrialStartResult(beginTrialStart(initialTrialState(true)), {
    ...active,
    phase: "error",
    message,
    sentEvents: "0",
    receivedEvents: "0",
    nativeArrivals: { keyboard: 0, button: 0, movement: 0, scroll: 0 },
  });
  assert.equal(state.message, message);
  assert.equal(state.sentEvents, "0");
  assert.equal(state.receivedEvents, "0");
  assert.equal(canStartTrial(state), false);
});

test("keyboard controls are available only before input or after known cleanup", () => {
  const prepared = initialTrialState(true);
  assert.equal(canNavigateTrialControls(prepared), true);
  assert.equal(canNavigateTrialControls(beginTrialStart(prepared)), false);
  assert.equal(canNavigateTrialControls({ ...active, phase: "active" }), false);
  assert.equal(canNavigateTrialControls({ ...active, phase: "stopping", busy: true }), false);
  assert.equal(
    canNavigateTrialControls({ ...active, phase: "finished", remainingSeconds: 0 }),
    true,
  );
  const busyError = applyTrialStartResult(beginTrialStart(prepared), {
    ...active,
    phase: "error",
    busy: true,
  });
  assert.equal(busyError.statusLost, false);
  assert.equal(canStopTrial(busyError), true);
  assert.equal(shouldPollTrial(busyError), true);
  assert.equal(canNavigateTrialControls(busyError), false);
  assert.equal(canNavigateTrialControls({ ...active, phase: "error", statusLost: true }), false);
});

test("returning to setup stays pending until native cleanup closes the window", () => {
  const closing = beginTrialClose(beginTrialStart(initialTrialState(true)));
  assert.equal(closing.phase, "stopping");
  assert.equal(closing.closePending, true);
  assert.equal(closing.busy, true);
  assert.equal(canStartTrial(closing), false);
  assert.equal(canStopTrial(closing), false);
  assert.equal(canReturnToSetup(closing), false);
  assert.equal(canNavigateTrialControls(closing), false);
  assert.equal(shouldPollTrial(closing), false);
});

test("a failed return keeps mouse recovery available and blocks keyboard input", () => {
  const failed = closeFailed(
    beginTrialClose(beginTrialStart(initialTrialState(true))),
    "Cleanup could not be confirmed.",
  );
  assert.equal(failed.phase, "error");
  assert.equal(failed.closePending, false);
  assert.equal(failed.statusLost, true);
  assert.equal(canStopTrial(failed), true);
  assert.equal(canReturnToSetup(failed), true);
  assert.equal(canNavigateTrialControls(failed), false);

  const cleaned = applyTrialView(failed, {
    ...active,
    phase: "error",
    message: "The controlled test has stopped.",
    busy: false,
  });
  assert.equal(cleaned.statusLost, false);
  assert.equal(cleaned.closeError, "");
  assert.equal(canNavigateTrialControls(cleaned), true);
});

test("a rejected return allows pagehide to retry native cleanup", () => {
  const source = readFileSync(new URL("./trial.js", import.meta.url), "utf8");
  assert.match(
    source,
    /catch \(error\) \{\s+closeInFlight = false;\s+closeAttempted = false;\s+trial = closeFailed/,
  );
});

test("peerName is optional, bounded, and never empty", () => {
  assert.equal(normalizeTrialView({ ...active }).peerName, "the other computer");
  assert.equal(
    normalizeTrialView({ ...active, peerName: "  Office Windows PC \u0007" }).peerName,
    "Office Windows PC",
  );
  assert.equal(
    normalizeTrialView({ ...active, peerName: "x".repeat(80) }).peerName,
    "x".repeat(48),
  );
  assert.equal(normalizeTrialView({ ...active, peerName: "   " }).peerName, "the other computer");
  assert.equal(normalizeTrialView({ ...active, peerName: 7 }), null);
});

test("sendsInput is optional but must be boolean when present", () => {
  assert.equal(normalizeTrialView({ ...active }).sendsInput, null);
  assert.equal(normalizeTrialView({ ...active, sendsInput: true }).sendsInput, true);
  assert.equal(normalizeTrialView({ ...active, sendsInput: false }).sendsInput, false);
  assert.equal(normalizeTrialView({ ...active, sendsInput: "yes" }), null);
});
