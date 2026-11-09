import assert from "node:assert/strict";
import test from "node:test";

import { AIRDROP_STUTTER_COPY, UNIVERSAL_CONTROL_STUTTER_COPY } from "./airdrop-model.mjs";
import {
  LINK_NOTE_AFTER_MS,
  WIFI_UNSTABLE_COPY,
  WIFI_UNSTABLE_SLOWER_COPY,
  connectionUnstableCopy,
  linkWarning,
  networkNote,
} from "./link-note-model.mjs";

const wifi = { kind: "Wi-Fi" };
const ethernet = { kind: "Ethernet" };
const unsteady = (linkQuality, linkUnsteadyMs) => ({ linkQuality, linkUnsteadyMs });

test("a link degraded or bad for two seconds earns the note, a shorter stall does not", () => {
  for (const quality of ["degraded", "bad"]) {
    assert.equal(
      linkWarning(unsteady(quality, LINK_NOTE_AFTER_MS), wifi, "Office PC"),
      WIFI_UNSTABLE_SLOWER_COPY,
    );
    assert.equal(linkWarning(unsteady(quality, LINK_NOTE_AFTER_MS - 1), wifi, "Office PC"), null);
  }
});

test("a steady, unknown or malformed link shows nothing", () => {
  assert.equal(linkWarning(unsteady("healthy", 0), wifi, "Office PC"), null);
  assert.equal(linkWarning(unsteady(null, 0), wifi, "Office PC"), null);
  assert.equal(linkWarning(null, wifi, "Office PC"), null);
  assert.equal(linkWarning(undefined, wifi, "Office PC"), null);
  assert.equal(linkWarning(unsteady("degraded", "5000"), wifi, "Office PC"), null);
  assert.equal(linkWarning(unsteady("worse", 5000), wifi, "Office PC"), null);
});

test("only a computer sharing over Wi-Fi blames its Wi-Fi; a wired one names the connection", () => {
  assert.equal(linkWarning(unsteady("bad", 9000), wifi, "Office PC"), WIFI_UNSTABLE_SLOWER_COPY);
  const wired = linkWarning(unsteady("bad", 9000), ethernet, "Office PC");
  assert.match(wired, /^The connection to Office PC looks unstable\. MonHop is sending/);
  assert.doesNotMatch(wired, /Wi-Fi/);
  assert.match(wired, /The connection to Office PC looks unstable/);
  assert.equal(linkWarning(unsteady("bad", 9000), null, "Office PC"), wired);
});

test("only a capped link says MonHop sends less often", () => {
  for (const quality of ["degraded", "bad"]) {
    for (const copy of [
      linkWarning(unsteady(quality, 3000), wifi, "Studio Mac"),
      linkWarning(unsteady(quality, 3000), ethernet, "Studio Mac"),
    ]) {
      assert.match(copy, /MonHop is sending pointer updates less often until it steadies\.$/);
    }
  }
  assert.equal(linkWarning(unsteady("healthy", 3000), wifi, "Studio Mac"), WIFI_UNSTABLE_COPY);
  assert.equal(
    linkWarning(unsteady("healthy", 3000), ethernet, "Studio Mac"),
    connectionUnstableCopy("Studio Mac"),
  );
  assert.doesNotMatch(WIFI_UNSTABLE_COPY, /less often/);
});

test("a card shows one note at most: AirDrop, then Universal Control, then the link", () => {
  const universal = UNIVERSAL_CONTROL_STUTTER_COPY;
  assert.equal(
    networkNote(AIRDROP_STUTTER_COPY, universal, WIFI_UNSTABLE_COPY),
    AIRDROP_STUTTER_COPY,
  );
  assert.equal(networkNote(AIRDROP_STUTTER_COPY, null, WIFI_UNSTABLE_COPY), AIRDROP_STUTTER_COPY);
  assert.equal(networkNote(null, universal, WIFI_UNSTABLE_COPY), universal);
  assert.equal(networkNote(null, universal, null), universal);
  assert.equal(networkNote(null, null, WIFI_UNSTABLE_COPY), WIFI_UNSTABLE_COPY);
  assert.equal(networkNote(AIRDROP_STUTTER_COPY, null, null), AIRDROP_STUTTER_COPY);
  assert.equal(networkNote(null, null, null), null);
});
