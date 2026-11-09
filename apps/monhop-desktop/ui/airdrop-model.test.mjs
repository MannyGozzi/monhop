import assert from "node:assert/strict";
import test from "node:test";

import {
  AIRDROP_STUTTER_COPY,
  UNIVERSAL_CONTROL_STUTTER_COPY,
  airdropWarning,
  normalizeAirDropView,
  universalControlWarning,
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

test("Universal Control warns only a Mac on Wi-Fi whose setting and link both agree", () => {
  assert.equal(
    universalControlWarning("macos", universalOn, wifi, hopping),
    UNIVERSAL_CONTROL_STUTTER_COPY,
  );
  assert.equal(universalControlWarning("windows", universalOn, wifi, hopping), null);
  assert.equal(universalControlWarning("macos", universalOn, { kind: "Ethernet" }, hopping), null);
  assert.equal(universalControlWarning("macos", universalOn, null, hopping), null);
  assert.equal(universalControlWarning("macos", { universalControl: false }, wifi, hopping), null);
  assert.equal(universalControlWarning("macos", undefined, wifi, hopping), null);
  assert.equal(universalControlWarning("macos", universalOn, wifi, { linkHopping: false }), null);
  assert.equal(universalControlWarning("macos", universalOn, wifi, { linkHopping: "true" }), null);
  assert.equal(universalControlWarning("macos", universalOn, wifi, null), null);
});

test("the Universal Control note names the exact switch", () => {
  assert.equal(
    UNIVERSAL_CONTROL_STUTTER_COPY,
    "Wi-Fi keeps pausing on this connection, and Universal Control on this Mac can cause that. If you don't use it, turn off “Allow your pointer and keyboard to move between any nearby Mac or iPad” in System Settings > Displays > Advanced.",
  );
});

test("the warning names the effect, the cause and both ways out", () => {
  assert.match(AIRDROP_STUTTER_COPY, /pointer stutter/);
  assert.match(AIRDROP_STUTTER_COPY, /AirDrop is set to Everyone/);
  assert.match(AIRDROP_STUTTER_COPY, /Contacts Only in Control Center/);
  assert.match(AIRDROP_STUTTER_COPY, /cable/);
});
