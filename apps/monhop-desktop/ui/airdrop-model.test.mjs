import assert from "node:assert/strict";
import test from "node:test";

import {
  AIRDROP_STUTTER_COPY,
  CONTINUITY_STUTTER_COPY,
  HANDOFF_STUTTER_COPY,
  airdropWarning,
  hoppingWarning,
  normalizeAirDropView,
} from "./airdrop-model.mjs";

const wifi = { kind: "Wi-Fi" };
const universalOn = { everyone: false, universalControl: true };
const hopping = { linkHopping: true };

test("only a Mac sharing over Wi-Fi with AirDrop open to everyone is warned", () => {
  const everyone = { everyone: true };
  assert.equal(airdropWarning("macos", everyone, wifi), AIRDROP_STUTTER_COPY);
  assert.equal(airdropWarning("windows", everyone, wifi), null);
  assert.equal(airdropWarning("macos", { everyone: false }, wifi), null);
  assert.equal(airdropWarning("macos", undefined, wifi), null);
  assert.equal(airdropWarning("macos", everyone, { kind: "Ethernet" }), null);
  assert.equal(airdropWarning("macos", everyone, null), null);
});

test("anything but a plain true reads as off", () => {
  assert.deepEqual(normalizeAirDropView({ everyone: true, universalControl: true }), {
    everyone: true,
    universalControl: true,
  });
  assert.deepEqual(normalizeAirDropView({ everyone: "true", universalControl: 1 }), {
    everyone: false,
    universalControl: false,
  });
  assert.deepEqual(normalizeAirDropView(null), { everyone: false, universalControl: false });
});

test("a Mac on Wi-Fi whose link keeps hopping is warned whatever Universal Control's setting", () => {
  assert.equal(hoppingWarning("macos", universalOn, wifi, hopping), CONTINUITY_STUTTER_COPY);
  assert.equal(
    hoppingWarning("macos", { universalControl: false }, wifi, hopping),
    HANDOFF_STUTTER_COPY,
  );
  assert.equal(hoppingWarning("macos", undefined, wifi, hopping), HANDOFF_STUTTER_COPY);
  assert.equal(hoppingWarning("windows", universalOn, wifi, hopping), null);
  assert.equal(hoppingWarning("macos", universalOn, { kind: "Ethernet" }, hopping), null);
  assert.equal(hoppingWarning("macos", universalOn, null, hopping), null);
  assert.equal(hoppingWarning("macos", universalOn, wifi, { linkHopping: false }), null);
  assert.equal(hoppingWarning("macos", universalOn, wifi, { linkHopping: "true" }), null);
  assert.equal(hoppingWarning("macos", universalOn, wifi, null), null);
});

test("the hopping notes say a router will not help and name each switch", () => {
  for (const copy of [HANDOFF_STUTTER_COPY, CONTINUITY_STUTTER_COPY]) {
    assert.match(copy, /about twice a second/);
    assert.match(copy, /faster router can't fix/);
    assert.match(copy, /Allow Handoff in System Settings > General > AirDrop & Handoff/);
  }
  assert.doesNotMatch(HANDOFF_STUTTER_COPY, /Universal Control/);
  assert.match(CONTINUITY_STUTTER_COPY, /System Settings > Displays > Advanced/);
});

test("the warning names the effect, the cause and both ways out", () => {
  assert.match(AIRDROP_STUTTER_COPY, /pointer stutter/);
  assert.match(AIRDROP_STUTTER_COPY, /AirDrop is set to Everyone/);
  assert.match(AIRDROP_STUTTER_COPY, /Contacts Only in Control Center/);
  assert.match(AIRDROP_STUTTER_COPY, /cable/);
});
