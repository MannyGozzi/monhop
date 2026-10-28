import assert from "node:assert/strict";
import test from "node:test";

import { AIRDROP_STUTTER_COPY, airdropWarning, normalizeAirDropView } from "./airdrop-model.mjs";

const wifi = { kind: "Wi-Fi" };

test("only a Mac sharing over Wi-Fi with AirDrop open to everyone is warned", () => {
  const everyone = { everyone: true };
  assert.equal(airdropWarning("macos", everyone, wifi), AIRDROP_STUTTER_COPY);
  assert.equal(airdropWarning("windows", everyone, wifi), null);
  assert.equal(airdropWarning("macos", { everyone: false }, wifi), null);
  assert.equal(airdropWarning("macos", undefined, wifi), null);
  assert.equal(airdropWarning("macos", everyone, { kind: "Ethernet" }), null);
  assert.equal(airdropWarning("macos", everyone, null), null);
});

test("anything but a plain true reads as not open to everyone", () => {
  assert.deepEqual(normalizeAirDropView({ everyone: true }), { everyone: true });
  assert.deepEqual(normalizeAirDropView({ everyone: "true" }), { everyone: false });
  assert.deepEqual(normalizeAirDropView(null), { everyone: false });
});

test("the warning names the effect, the cause and both ways out", () => {
  assert.match(AIRDROP_STUTTER_COPY, /pointer stutter/);
  assert.match(AIRDROP_STUTTER_COPY, /AirDrop is set to Everyone/);
  assert.match(AIRDROP_STUTTER_COPY, /Contacts Only in Control Center/);
  assert.match(AIRDROP_STUTTER_COPY, /cable/);
});
