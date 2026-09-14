import assert from "node:assert/strict";
import test from "node:test";

import {
  applyPairingView,
  canConfirmPairing,
  editCandidateCode,
  formatFingerprint,
  initialPairingState,
  isBusyPairing,
  localNetworkStatus,
  invalidatePairingCandidate,
  normalizePairingView,
  setFingerprintCompared,
} from "./pairing-model.mjs";

const localFingerprint = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const peerFingerprint = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

test("a saved computer is no longer a pairing phase; pairing only ever ends at paired", () => {
  assert.equal(normalizePairingView({ phase: "saved", storageOutcome: "verified" }).phase, "error");
  assert.equal(normalizePairingView({ phase: "ready" }).storageOutcome, "unverified");
  assert.equal(
    normalizePairingView({ phase: "ready", storageOutcome: "unchanged" }).storageOutcome,
    "unchanged",
  );
});

test("unknown native results become visible errors and unverified storage stays explicit", () => {
  assert.deepEqual(normalizePairingView({ phase: "not-a-phase" }), {
    phase: "error",
    localCode: null,
    localFingerprint: null,
    peerFingerprint: null,
    peerAddress: null,
    candidateId: null,
    role: null,
    storageOutcome: "unverified",
    message: "The pairing state was not recognized. Reload pairing.",
    busy: false,
    networkAccess: "not-verified",
    networkAccessMessage: "",
    localPlatform: null,
    peerPlatform: null,
  });
  const platforms = normalizePairingView({
    phase: "review",
    localPlatform: "macos",
    peerPlatform: "macos",
  });
  assert.equal(platforms.localPlatform, "macos");
  assert.equal(platforms.peerPlatform, "macos");
  assert.equal(normalizePairingView({ phase: "review", peerPlatform: "linux" }).peerPlatform, null);
});

test("editing code or changing networks revokes a stale review confirmation", () => {
  let state = applyPairingView(initialPairingState(), {
    phase: "review",
    localFingerprint,
    peerFingerprint,
    candidateId: 7,
    role: "connect",
  });
  state = setFingerprintCompared(state, true);
  assert.equal(canConfirmPairing(state), true);

  state = editCandidateCode(state, "updated candidate code");
  assert.equal(state.compared, false);
  assert.equal(state.candidateStale, true);
  assert.equal(canConfirmPairing(state), false);

  state = invalidatePairingCandidate(
    setFingerprintCompared({ ...state, candidateStale: false }, true),
  );
  assert.equal(state.compared, false);
  assert.equal(state.candidateStale, true);
  assert.equal(canConfirmPairing(state), false);
});

test("fingerprints remain complete and readable without accepting abbreviated values", () => {
  assert.equal(
    formatFingerprint(localFingerprint),
    "0123 4567 89ab cdef 0123 4567 89ab cdef 0123 4567 89ab cdef 0123 4567 89ab cdef",
  );
  assert.equal(formatFingerprint(localFingerprint.slice(0, 16)), "Not reported");
});

test("fingerprint confirmation requires an explicit match", () => {
  const review = applyPairingView(initialPairingState(), {
    phase: "review",
    candidateId: 7,
    role: "connect",
    localFingerprint,
    peerFingerprint,
  });

  for (const value of [false, undefined, "true"]) {
    assert.equal(canConfirmPairing(setFingerprintCompared(review, value)), false);
  }
  assert.equal(canConfirmPairing(setFingerprintCompared(review, true)), true);
});

test("confirming needs a complete native identity, a role, and an explicit fingerprint match", () => {
  const value = {
    phase: "review",
    candidateId: 9,
    role: "listen",
    localFingerprint,
    peerFingerprint,
  };
  const compared = (patch) =>
    canConfirmPairing(
      setFingerprintCompared(applyPairingView(initialPairingState(), { ...value, ...patch }), true),
    );
  assert.equal(compared({}), true);
  for (const patch of [
    { localFingerprint: null },
    { peerFingerprint: "abcd" },
    { role: null },
    { candidateId: null },
    { busy: true },
    { phase: "paired", storageOutcome: "verified" },
  ])
    assert.equal(compared(patch), false, JSON.stringify(patch));
});

test("partial native success cannot display a verified pairing", () => {
  assert.equal(normalizePairingView({ phase: "paired" }).phase, "error");
  assert.equal(
    normalizePairingView({
      phase: "paired",
      localFingerprint,
      peerFingerprint,
      storageOutcome: "unverified",
    }).phase,
    "error",
  );
  assert.equal(
    normalizePairingView({
      phase: "paired",
      localFingerprint,
      peerFingerprint,
      storageOutcome: "verified",
    }).phase,
    "paired",
  );
});

test("network requests never become an allowed or denied permission verdict", () => {
  for (const [networkAccess, label] of [
    ["attempted", "Request attempted"],
    ["incomplete", "Request incomplete"],
    ["allowed", "Checked when pairing"],
    ["denied", "Checked when pairing"],
  ]) {
    const state = applyPairingView(initialPairingState(), { phase: "review", networkAccess });
    assert.equal(localNetworkStatus(state).label, label);
    assert.equal(canConfirmPairing(state), false);
  }
  const requesting = applyPairingView(initialPairingState(), {
    phase: "requesting-network",
    networkAccess: "requesting",
    busy: true,
  });
  assert.equal(isBusyPairing(requesting), true);
  assert.equal(canConfirmPairing(requesting), false);
  const attempted = applyPairingView(initialPairingState(), {
    phase: "review",
    networkAccess: "attempted",
    candidateId: 1,
  });
  assert.equal(
    localNetworkStatus(invalidatePairingCandidate(attempted)).label,
    "Checked when pairing",
  );
});

test("fingerprints arrive lowercased, so a pairing view compares against the computer list directly", () => {
  const upper = "AB12".repeat(16);
  const view = normalizePairingView({
    phase: "paired",
    storageOutcome: "verified",
    localFingerprint: upper,
    peerFingerprint: upper,
  });
  assert.equal(view.phase, "paired");
  assert.equal(view.peerFingerprint, upper.toLowerCase());
  assert.equal(view.localFingerprint, upper.toLowerCase());
  assert.equal(normalizePairingView({ phase: "ready" }).peerFingerprint, null);
});
