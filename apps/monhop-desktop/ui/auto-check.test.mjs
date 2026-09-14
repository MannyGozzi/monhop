import assert from "node:assert/strict";
import test from "node:test";

import {
  canOpenPairingOnEntry,
  recordPairingOpenContext,
  selectedNetworkContextKey,
  shouldStartAutomaticSnapshot,
  snapshotCheckResult,
} from "./auto-check.mjs";

test("automatic setup checks are read-only startup, entry, and return signals", () => {
  assert.equal(shouldStartAutomaticSnapshot({ nativeAvailable: true, trigger: "startup" }), true);
  assert.equal(
    shouldStartAutomaticSnapshot({ nativeAvailable: true, trigger: "entry", hasSnapshot: false }),
    true,
  );
  assert.equal(
    shouldStartAutomaticSnapshot({ nativeAvailable: true, trigger: "entry", hasSnapshot: true }),
    true,
  );
  assert.equal(shouldStartAutomaticSnapshot({ nativeAvailable: true, trigger: "focus" }), true);
  assert.equal(shouldStartAutomaticSnapshot({ nativeAvailable: true, trigger: "poll" }), false);
  assert.equal(shouldStartAutomaticSnapshot({ nativeAvailable: false, trigger: "startup" }), false);
});

test("automatic check failures are sticky and burst or busy signals do not queue work", () => {
  const base = { nativeAvailable: true, trigger: "focus" };
  assert.equal(shouldStartAutomaticSnapshot({ ...base, checking: true }), false);
  assert.equal(shouldStartAutomaticSnapshot({ ...base, busy: true }), false);
  assert.equal(shouldStartAutomaticSnapshot({ ...base, automaticFailed: true }), false);
  assert.equal(shouldStartAutomaticSnapshot({ ...base, uiCheck: true }), false);
});

test("partial snapshot reports keep their data but require an explicit refresh", () => {
  const partial = snapshotCheckResult(
    { errors: ["Could not list displays"] },
    "Checked on launch.",
  );
  assert.deepEqual(partial, {
    automaticFailed: true,
    freshness: "Some checks did not finish. Use refresh to try again.",
  });
  assert.equal(
    shouldStartAutomaticSnapshot({
      nativeAvailable: true,
      trigger: "focus",
      automaticFailed: partial.automaticFailed,
    }),
    false,
  );
  assert.deepEqual(snapshotCheckResult({ errors: [] }, "Checked after requesting access."), {
    automaticFailed: false,
    freshness: "Checked after requesting access.",
  });
});

test("Pair opens once for an explicitly entered eligible selection context", () => {
  let attempted = new Set();
  const first = {
    nativeAvailable: true,
    eligible: true,
    contextKey: "en0:10.0.0.2",
    attemptedKeys: attempted,
  };
  assert.equal(canOpenPairingOnEntry(first), true);
  attempted = recordPairingOpenContext(attempted, first.contextKey);
  assert.equal(canOpenPairingOnEntry({ ...first, attemptedKeys: attempted }), false);
  assert.equal(
    canOpenPairingOnEntry({ ...first, contextKey: "en0:10.0.1.2", attemptedKeys: attempted }),
    true,
  );
  assert.equal(
    canOpenPairingOnEntry({
      ...first,
      contextKey: "en0:10.0.1.2",
      attemptedKeys: attempted,
      busy: true,
    }),
    false,
  );
  attempted = recordPairingOpenContext(attempted, "en0:10.0.1.2");
  assert.equal(
    canOpenPairingOnEntry({ ...first, contextKey: "en0:10.0.1.2", attemptedKeys: attempted }),
    false,
  );
});

test("network freshness invalidates a pairing review when any selected context field changes", () => {
  const selected = {
    id: "en0",
    address: "10.0.0.2",
    prefixLength: 24,
    networkName: "Office",
    attachmentKnown: true,
    up: true,
    physical: true,
  };
  const key = selectedNetworkContextKey(selected);
  for (const [field, value] of [
    ["address", "10.0.1.2"],
    ["prefixLength", 16],
    ["networkName", "Guest"],
    ["attachmentKnown", false],
    ["up", false],
    ["physical", false],
  ]) {
    assert.notEqual(selectedNetworkContextKey({ ...selected, [field]: value }), key, field);
  }
});
