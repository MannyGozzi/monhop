import assert from "node:assert/strict";
import test from "node:test";

import { homeEntries } from "./home-model.mjs";

const A = "a".repeat(64);
const B = "b".repeat(64);
const C = "c".repeat(64);

function computers(items, patch = {}) {
  return {
    loaded: true,
    items,
    active: null,
    enabled: [],
    paused: false,
    interfaceId: null,
    ...patch,
  };
}

function computer(fingerprint, patch = {}) {
  return { fingerprint, platform: "windows", name: "", address: null, setup: {}, ...patch };
}

test("nothing enabled: every computer is 'other', no hero and no group card", () => {
  const entries = homeEntries(computers([computer(A), computer(B)]), null, null);
  assert.deepEqual(
    entries.map((e) => e.type),
    ["other", "other"],
  );
});

test("exactly one enabled renders the same lone hero Home has always drawn, no group card", () => {
  const entries = homeEntries(computers([computer(A), computer(B)], { active: A }), null, A);
  assert.deepEqual(
    entries.map((e) => e.type),
    ["hero", "other"],
  );
  assert.equal(entries[0].computer.fingerprint, A);
  assert.equal(entries[1].computer.fingerprint, B);
});

test("two or more enabled get the group card first, then a live entry each, in the group's own order", () => {
  const list = computers([computer(A), computer(B), computer(C)]);
  const entries = homeEntries(list, { enabled: [B, A] }, null);
  assert.deepEqual(
    entries.map((e) => e.type),
    ["group", "live", "live", "other"],
  );
  assert.deepEqual(
    entries.slice(1, 3).map((e) => e.computer.fingerprint),
    [B, A],
  );
  assert.equal(entries[3].computer.fingerprint, C);
});

test("tone follows position in the enabled order, matching the arrangement picture's own tones", () => {
  const list = computers([computer(A), computer(B), computer(C)]);
  const entries = homeEntries(list, { enabled: [A, B, C] }, null);
  assert.deepEqual(
    entries.filter((e) => e.type === "live").map((e) => e.tone),
    ["peer", "peer-2", "peer-3"],
  );
});

test("the live view's own enabled list wins over the polled computers reply, which wins over a bare active fingerprint", () => {
  const list = computers([computer(A), computer(B)], { enabled: [A] });
  assert.deepEqual(
    homeEntries(list, { enabled: [B] }, A)
      .filter((e) => e.type !== "other")
      .map((e) => e.computer.fingerprint),
    [B],
  );
  assert.deepEqual(
    homeEntries(list, null, B)
      .filter((e) => e.type !== "other")
      .map((e) => e.computer.fingerprint),
    [A],
  );
  assert.deepEqual(
    homeEntries(computers([computer(A), computer(B)]), null, A)
      .filter((e) => e.type !== "other")
      .map((e) => e.computer.fingerprint),
    [A],
  );
});

test("a computer no one paired with anymore is never listed, however it is named", () => {
  const list = computers([computer(A)]);
  const entries = homeEntries(list, { enabled: [A, B] }, null);
  assert.deepEqual(
    entries.map((e) => e.type),
    ["hero"],
  );
  assert.equal(entries[0].computer.fingerprint, A);
});
