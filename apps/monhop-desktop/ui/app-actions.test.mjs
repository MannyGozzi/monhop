import assert from "node:assert/strict";
import test from "node:test";

import {
  clipboardContext,
  invokeSetComputerEnabled,
  isUnknownCommandError,
  legacySetActivePayload,
  pollFingerprint,
} from "./app-actions.mjs";

const FP = "a".repeat(64);

// ---------- isUnknownCommandError ----------

test("a Tauri command-not-found rejection naming the command is recognized", () => {
  assert.equal(
    isUnknownCommandError(
      new Error("Command sharing_set_enabled not found"),
      "sharing_set_enabled",
    ),
    true,
  );
  assert.equal(
    isUnknownCommandError("command sharing_set_enabled not found", "sharing_set_enabled"),
    true,
  );
});

test("a real application error is never mistaken for an unknown command", () => {
  assert.equal(
    isUnknownCommandError(new Error("That computer is not paired."), "sharing_set_enabled"),
    false,
  );
  assert.equal(
    isUnknownCommandError(new Error("Command sharing_set_active not found"), "sharing_set_enabled"),
    false,
  );
  assert.equal(isUnknownCommandError(new Error("not found"), "sharing_set_enabled"), false);
});

test("a non-Error rejection is stringified before matching", () => {
  assert.equal(isUnknownCommandError(null, "sharing_set_enabled"), false);
  assert.equal(isUnknownCommandError(undefined, "sharing_set_enabled"), false);
});

// ---------- legacySetActivePayload ----------

test("switching a computer on sends its fingerprint, off sends null", () => {
  assert.deepEqual(legacySetActivePayload(FP, true, "iface"), {
    fingerprint: FP,
    interfaceId: "iface",
  });
  assert.deepEqual(legacySetActivePayload(FP, false, "iface"), {
    fingerprint: null,
    interfaceId: "iface",
  });
});

// ---------- invokeSetComputerEnabled ----------

test("a successful sharing_set_enabled call never falls back", async () => {
  const calls = [];
  const invoke = async (command, payload) => {
    calls.push([command, payload]);
    return { ok: command };
  };
  const result = await invokeSetComputerEnabled(invoke, FP, true, "iface");
  assert.deepEqual(calls, [
    ["sharing_set_enabled", { fingerprint: FP, enabled: true, interfaceId: "iface" }],
  ]);
  assert.deepEqual(result, { ok: "sharing_set_enabled" });
});

test("an unknown-command rejection retries with sharing_set_active", async () => {
  const calls = [];
  const invoke = async (command, payload) => {
    calls.push([command, payload]);
    if (command === "sharing_set_enabled") throw new Error("Command sharing_set_enabled not found");
    return { ok: command };
  };
  const result = await invokeSetComputerEnabled(invoke, FP, false, "iface");
  assert.deepEqual(calls, [
    ["sharing_set_enabled", { fingerprint: FP, enabled: false, interfaceId: "iface" }],
    ["sharing_set_active", { fingerprint: null, interfaceId: "iface" }],
  ]);
  assert.deepEqual(result, { ok: "sharing_set_active" });
});

async function invokeRejectingWithApplicationError(command) {
  if (command === "sharing_set_enabled") throw new Error("That computer is not paired.");
  throw new Error("should never reach sharing_set_active");
}

test("a real failure from sharing_set_enabled propagates instead of being swallowed", async () => {
  await assert.rejects(
    () => invokeSetComputerEnabled(invokeRejectingWithApplicationError, FP, true, "iface"),
    /not paired/,
  );
});

// ---------- pollFingerprint ----------

test("the bare active fingerprint wins when the backend still sends one", () => {
  assert.equal(pollFingerprint({ active: FP, enabled: [] }), FP);
});

test("the first enabled computer covers a backend that only sends the enabled list", () => {
  assert.equal(pollFingerprint({ active: null, enabled: [FP, "b".repeat(64)] }), FP);
});

test("nothing switched in polls for nothing", () => {
  assert.equal(pollFingerprint({ active: null, enabled: [] }), null);
  assert.equal(pollFingerprint(undefined), null);
});

// ---------- clipboardContext ----------

test("an undefined view (never proven, or an older backend) hides the card", () => {
  assert.equal(clipboardContext(undefined, false), undefined);
});

test("any read view, even an empty-looking one, is passed through with its pending flag", () => {
  assert.deepEqual(clipboardContext({ enabled: false }, false), {
    view: { enabled: false },
    pending: false,
  });
  assert.deepEqual(clipboardContext(null, true), { view: null, pending: true });
});
