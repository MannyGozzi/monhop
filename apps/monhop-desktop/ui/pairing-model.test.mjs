import assert from "node:assert/strict";
import test from "node:test";

import {
  CODE_LENGTH,
  applyPairingView,
  canStartPairing,
  canSubmitPairingCode,
  choosePairingMode,
  codeGroups,
  codeSymbols,
  codeTimeLeft,
  countdownProgress,
  editPairingEntry,
  failPairingEntry,
  formatCodeInput,
  formatCountdown,
  initialPairingState,
  isBusyPairing,
  isPairingAttemptLive,
  normalizePairingView,
  pairingErrorPresentation,
  pairingStep,
  spokenCode,
} from "./pairing-model.mjs";

const localFingerprint = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const peerFingerprint = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const CODE = "7KQ4-M9XR-2HTW";

const paired = (patch = {}) => ({
  phase: "paired",
  storageOutcome: "verified",
  localFingerprint,
  peerFingerprint,
  peerPlatform: "windows",
  badge: { color: 0, symbols: [0, 21, 19] },
  ...patch,
});

test("unknown native results become visible errors and unverified storage stays explicit", () => {
  assert.deepEqual(normalizePairingView({ phase: "not-a-phase" }), {
    phase: "error",
    role: null,
    code: null,
    codeExpiresAtMs: null,
    message: "The pairing state was not recognized. Reload pairing.",
    busy: false,
    localFingerprint: null,
    peerFingerprint: null,
    peerAddress: null,
    localPlatform: null,
    peerPlatform: null,
    badge: null,
    storageOutcome: "unverified",
  });
  for (const phase of ["review", "waiting", "requesting-network", "saved"])
    assert.equal(normalizePairingView({ phase }).phase, "error", phase);
  assert.equal(normalizePairingView({ phase: "ready", peerPlatform: "linux" }).peerPlatform, null);
  assert.equal(normalizePairingView({ phase: "ready", role: "listen" }).role, null);
  assert.equal(normalizePairingView({ phase: "error", role: "entering" }).role, "entering");
});

test("a code is only carried while it is shown, and only in its display form", () => {
  const shown = normalizePairingView({ phase: "showing", code: CODE, codeExpiresAtMs: 5000 });
  assert.equal(shown.code, CODE);
  assert.equal(shown.codeExpiresAtMs, 5000);
  assert.equal(normalizePairingView({ phase: "error", code: CODE }).code, null);
  for (const code of ["7kq4-m9xr-2htw", "7KQ47M9XR2HTW", "7KQ4-M9XR-2HTU", "7KQ4-M9XR-2HT", 12])
    assert.equal(normalizePairingView({ phase: "showing", code }).code, null, String(code));
  assert.equal(
    normalizePairingView({ phase: "showing", code: null, codeExpiresAtMs: 5000 }).codeExpiresAtMs,
    null,
  );
});

test("partial native success cannot display a verified pairing, and a badge comes only with it", () => {
  assert.equal(normalizePairingView({ phase: "paired" }).phase, "error");
  assert.equal(normalizePairingView(paired({ storageOutcome: "unverified" })).phase, "error");
  assert.equal(normalizePairingView(paired({ localFingerprint: null })).phase, "error");
  const view = normalizePairingView(paired());
  assert.equal(view.phase, "paired");
  assert.deepEqual(view.badge, { color: 0, symbols: [0, 21, 19] });
  assert.equal(
    normalizePairingView(paired({ badge: { color: 8, symbols: [0, 1, 2] } })).badge,
    null,
  );
  assert.equal(
    normalizePairingView({ phase: "verifying", badge: { color: 1, symbols: [0, 1, 2] } }).badge,
    null,
  );
});

test("fingerprints arrive lowercased, so a pairing view compares against the computer list directly", () => {
  const upper = "AB12".repeat(16);
  const view = normalizePairingView(
    paired({ localFingerprint: upper, peerFingerprint: upper.toLowerCase() }),
  );
  assert.equal(view.phase, "paired");
  assert.equal(view.localFingerprint, upper.toLowerCase());
});

test("typing formats the code: case, spaces and dashes never matter and look-alikes are mapped", () => {
  assert.deepEqual(formatCodeInput("7kq4 m9xr 2htw"), {
    value: CODE,
    caret: CODE.length,
    symbols: "7KQ4M9XR2HTW",
  });
  assert.equal(formatCodeInput("7KQ4-M9XR-2HTW").value, CODE);
  assert.equal(formatCodeInput("o1il").value, "0111");
  assert.equal(formatCodeInput("7KQ4M").value, "7KQ4-M");
  assert.equal(formatCodeInput("7KQ4").value, "7KQ4");
  // U is never part of a code, and anything past twelve symbols is dropped.
  assert.equal(formatCodeInput("U#7KQ4M9XR2HTWZZZ").value, CODE);
  assert.equal(codeSymbols("x".repeat(80) + "7KQ4"), "XXXXXXXXXXXX");
  assert.equal(codeSymbols(` ${"-".repeat(64)}7KQ4`), "");
});

test("the caret stays after the symbol it followed while dashes move around it", () => {
  // Typing the fifth symbol at the end puts the caret after the new dash and symbol.
  assert.equal(formatCodeInput("7KQ4M", 5).caret, 6);
  // A caret right after the fourth symbol stays before the dash.
  assert.equal(formatCodeInput("7KQ4M9", 4).caret, 4);
  // An insertion in the middle keeps the caret after the inserted symbol.
  const inserted = formatCodeInput("7KQ4-AM9XR", 6);
  assert.equal(inserted.value, "7KQ4-AM9X-R");
  assert.equal(inserted.caret, 6);
  assert.equal(formatCodeInput("", 0).caret, 0);
});

test("Connect needs exactly twelve symbols and a computer that can start pairing", () => {
  let state = applyPairingView(initialPairingState(), { phase: "ready" });
  state = choosePairingMode(state, "enter");
  state = editPairingEntry(state, "7KQ4M9XR2HT");
  assert.equal(canSubmitPairingCode(state), false);
  state = editPairingEntry(state, "7kq4m9xr2htw");
  assert.equal(state.entry, CODE);
  assert.equal(canSubmitPairingCode(state), true);
  assert.equal(
    canSubmitPairingCode({ ...state, view: { ...state.view, phase: "connecting" } }),
    false,
  );
  assert.equal(canSubmitPairingCode({ ...state, view: { ...state.view, busy: true } }), false);
  assert.equal(codeSymbols(state.entry).length, CODE_LENGTH);
});

test("a local error stays under the field without clearing it, and typing clears the error", () => {
  let state = choosePairingMode(
    applyPairingView(initialPairingState(), { phase: "ready" }),
    "enter",
  );
  state = editPairingEntry(state, CODE);
  state = failPairingEntry(state, "That code has a typo. Check it and try again.");
  assert.equal(state.entry, CODE);
  assert.equal(state.entryError, "That code has a typo. Check it and try again.");
  assert.equal(pairingStep(state), "enter");
  state = editPairingEntry(state, "7KQ4-M9XR-2HT");
  assert.equal(state.entryError, "");
});

test("the screen follows the phase, and the user's pick only applies while nothing runs", () => {
  const now = 10_000;
  const at = (view, mode = null) =>
    pairingStep({ ...applyPairingView(initialPairingState(), view, now), mode }, now);
  assert.equal(pairingStep(initialPairingState()), "closed");
  assert.equal(at({ phase: "identity-missing" }), "identity");
  assert.equal(at({ phase: "ready" }), "choose");
  assert.equal(at({ phase: "ready" }, "enter"), "enter");
  assert.equal(at({ phase: "showing", code: CODE, codeExpiresAtMs: now + 1 }), "show");
  // Binding can lag: the code arrives on a later status.
  assert.equal(at({ phase: "showing", busy: true }), "show");
  assert.equal(at({ phase: "showing", code: CODE, codeExpiresAtMs: now }), "expired");
  for (const phase of ["connecting", "verifying", "saving", "stopping"])
    assert.equal(at({ phase, busy: true }), "progress", phase);
  assert.equal(at(paired()), "paired");
  assert.equal(at(paired(), "choose"), "choose");
  assert.equal(at({ phase: "error", role: "entering" }), "error");
  assert.equal(at({ phase: "error", role: "entering" }, "enter"), "enter");
  assert.equal(at({ phase: "error" }, "choose"), "choose");
});

test("a new attempt drops the user's pick, and pairing clears the typed code", () => {
  let state = editPairingEntry(
    choosePairingMode(applyPairingView(initialPairingState(), { phase: "ready" }), "enter"),
    CODE,
  );
  state = applyPairingView(state, { phase: "connecting", role: "entering", busy: true });
  assert.equal(state.mode, null);
  assert.equal(state.entry, CODE);
  state = applyPairingView(state, paired());
  assert.equal(state.entry, "");
  // Entering again after a failure starts from an empty field.
  state = applyPairingView(state, { phase: "error", role: "entering", message: "No match." });
  state = editPairingEntry(state, CODE);
  assert.equal(choosePairingMode(state, "enter").entry, "");
});

const view = (phase, busy = false) => applyPairingView(initialPairingState(), { phase, busy });

test("busy and live phases match the backend's busy flag", () => {
  for (const phase of ["showing", "connecting", "verifying", "saving", "stopping"])
    assert.equal(isBusyPairing(view(phase, true)), true, phase);
  for (const phase of ["ready", "paired", "error", "identity-missing"])
    assert.equal(isBusyPairing(view(phase)), false, phase);
  assert.equal(isBusyPairing(view("ready", true)), true);
  assert.equal(isPairingAttemptLive(view("showing", true)), true);
  assert.equal(isPairingAttemptLive(view("stopping", true)), false);
  assert.equal(canStartPairing(view("error")), true);
  assert.equal(canStartPairing(view("showing", true)), false);
  assert.equal(canStartPairing(view("identity-missing")), false);
});

test("a shown code is grouped, spelled out for screen readers, and counted down", () => {
  assert.deepEqual(codeGroups(CODE), ["7KQ4", "M9XR", "2HTW"]);
  assert.deepEqual(codeGroups("nope"), []);
  assert.equal(spokenCode(CODE), "7 K Q 4, M 9 X R, 2 H T W");
  assert.equal(formatCountdown(120_000), "2:00");
  assert.equal(formatCountdown(61_001), "1:02");
  assert.equal(formatCountdown(9_000), "0:09");
  assert.equal(formatCountdown(-5), "0:00");
  assert.equal(codeTimeLeft({ codeExpiresAtMs: 5_000 }, 7_000), 0);
  assert.equal(codeTimeLeft({ codeExpiresAtMs: null }, 7_000), null);
});

test("the countdown counts from when the code first appeared, not from each status poll", () => {
  let state = applyPairingView(
    initialPairingState(),
    { phase: "showing", code: CODE, codeExpiresAtMs: 120_000 },
    0,
  );
  state = applyPairingView(
    state,
    { phase: "showing", code: CODE, codeExpiresAtMs: 120_000 },
    30_000,
  );
  assert.equal(state.codeShownAt, 0);
  assert.deepEqual(countdownProgress(state, 30_000), {
    total: 120_000,
    elapsed: 30_000,
    left: 0.75,
  });
  // A new code starts a new window.
  state = applyPairingView(
    state,
    { phase: "showing", code: "ABCD-EFGH-JKMN", codeExpiresAtMs: 160_000 },
    40_000,
  );
  assert.equal(state.codeShownAt, 40_000);
  assert.equal(applyPairingView(state, { phase: "verifying", busy: true }).codeShownAt, null);
  assert.equal(countdownProgress(initialPairingState()), null);
});

test("errors use the backend's sentence, and the role picks the way forward", () => {
  const wrong = normalizePairingView({
    phase: "error",
    role: "showing",
    message: "Someone entered a wrong code. This code no longer works.",
  });
  assert.deepEqual(pairingErrorPresentation(wrong), {
    title: "Pairing did not finish",
    detail: "Someone entered a wrong code. This code no longer works.",
    retry: "new-code",
  });
  assert.equal(
    pairingErrorPresentation(normalizePairingView({ phase: "error", role: "entering" })).retry,
    "enter-again",
  );
  assert.equal(pairingErrorPresentation(normalizePairingView({ phase: "error" })).retry, "reopen");
});
