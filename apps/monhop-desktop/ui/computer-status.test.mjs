import assert from "node:assert/strict";
import test from "node:test";

import { activeStatus, computerStatus, isLiveStatus, isSessionStatus } from "./computer-status.mjs";
import { initialComputers, normalizeComputers } from "./computers-model.mjs";

const WINDOWS = "b".repeat(64);
const MAC = "c".repeat(64);

const computer = { fingerprint: WINDOWS, name: "Noctua Windows PC", platform: "windows" };
const other = { fingerprint: MAC, name: "Studio Mac", platform: "macos" };

function view(patch = {}) {
  return {
    phase: "off",
    peerFingerprint: null,
    active: null,
    editing: false,
    sharingRole: null,
    lastFailure: "",
    message: "",
    ...patch,
  };
}

test("a computer nobody is using reads as paired, whatever the live connection is doing", () => {
  const live = view({ phase: "sharing", peerFingerprint: WINDOWS, active: WINDOWS });
  const standby = computerStatus(other, live, WINDOWS);
  assert.equal(standby.key, "standby");
  assert.equal(standby.label, "Paired");
  assert.equal(standby.tone, "neutral");
  assert.equal(isLiveStatus(standby), false);
});

test("the computer in use follows the live phase, and says which way input travels", () => {
  const sends = computerStatus(
    computer,
    view({ phase: "sharing", peerFingerprint: WINDOWS, active: WINDOWS, sharingRole: "sends" }),
    WINDOWS,
  );
  assert.equal(sends.label, "Sharing input");
  assert.equal(sends.tone, "active");
  assert.equal(sends.detail, "Your keyboard and mouse reach Noctua Windows PC");
  assert.equal(isLiveStatus(sends), true);

  const receives = computerStatus(
    computer,
    view({ phase: "sharing", peerFingerprint: WINDOWS, active: WINDOWS, sharingRole: "receives" }),
    WINDOWS,
  );
  assert.equal(receives.detail, "Noctua Windows PC's keyboard and mouse reach this computer");
});

test("a session waiting out a silence reads as reconnecting, and stays live", () => {
  const held = computerStatus(
    computer,
    view({
      phase: "sharing",
      peerFingerprint: WINDOWS,
      active: WINDOWS,
      sharingRole: "sends",
      held: true,
    }),
    WINDOWS,
  );
  assert.equal(held.key, "reconnecting");
  assert.equal(held.label, "Reconnecting…");
  assert.equal(held.tone, "checking");
  assert.match(held.detail, /Input stays on this computer/);
  assert.equal(isLiveStatus(held), true);
  assert.equal(isSessionStatus(held), true);
  assert.equal(
    isSessionStatus(
      computerStatus(
        computer,
        view({ phase: "connected", peerFingerprint: WINDOWS, active: WINDOWS }),
        WINDOWS,
      ),
    ),
    false,
  );
});

test("the setup link reads as connected, and says when it is being arranged", () => {
  const base = { phase: "connected", peerFingerprint: WINDOWS, active: WINDOWS };
  const connected = computerStatus(computer, view(base), WINDOWS);
  assert.equal(connected.key, "connected");
  assert.equal(connected.label, "Connected");
  assert.match(connected.detail, /Arrange the displays/);
  assert.equal(isLiveStatus(connected), true);

  const editing = computerStatus(computer, view({ ...base, editing: true }), WINDOWS);
  assert.equal(editing.key, "editing");
  assert.equal(editing.label, "Connected");
  assert.match(editing.detail, /Sharing resumes after you apply/);
  assert.equal(isLiveStatus(editing), true);
});

test("dialing, stopping and failing each get one honest line", () => {
  for (const phase of ["connecting", "reconnecting", "starting"]) {
    const status = computerStatus(
      computer,
      view({ phase, peerFingerprint: WINDOWS, active: WINDOWS }),
      WINDOWS,
    );
    assert.equal(status.label, "Connecting…", phase);
    assert.equal(status.tone, "checking", phase);
    assert.equal(isLiveStatus(status), false, phase);
  }
  const dropped = computerStatus(
    computer,
    view({
      phase: "connecting",
      peerFingerprint: WINDOWS,
      active: WINDOWS,
      lastFailure: "Wire closed.",
    }),
    WINDOWS,
  );
  // Home shows the drop line once, as its own note; the status line never repeats it.
  assert.equal(dropped.detail, "Reaching Noctua Windows PC.");

  const stopping = computerStatus(
    computer,
    view({ phase: "stopping", peerFingerprint: WINDOWS, active: WINDOWS }),
    WINDOWS,
  );
  assert.equal(stopping.label, "Stopping…");

  for (const phase of ["error", "unknown"]) {
    const failed = computerStatus(
      computer,
      view({ phase, active: WINDOWS, message: "The other computer refused." }),
      WINDOWS,
    );
    assert.equal(failed.label, "Can't connect", phase);
    assert.equal(failed.tone, "error", phase);
    assert.equal(failed.detail, "The other computer refused.", phase);
  }
});

test("the computer in use with no worker yet is still on its way, never paused", () => {
  const dialing = computerStatus(computer, view({ phase: "off", active: WINDOWS }), WINDOWS);
  assert.equal(dialing.key, "connecting");
  assert.equal(dialing.detail, "Reaching Noctua Windows PC.");
});

test("the header pill names the computer in use, or why there is nothing to report", () => {
  const computers = normalizeComputers({
    computers: [
      { fingerprint: WINDOWS, name: "Noctua Windows PC", platform: "windows" },
      { fingerprint: MAC, name: "Studio Mac", platform: "macos" },
    ],
    active: WINDOWS,
  });
  assert.equal(
    activeStatus({ computers, sharingView: null, active: null, nativeAvailable: false }).label,
    "Preview only",
  );
  const none = activeStatus({ computers, sharingView: null, active: null, nativeAvailable: true });
  assert.equal(none.label, "No computer");
  assert.equal(none.detail, "Choose a computer to use.");
  assert.equal(
    activeStatus({
      computers: initialComputers(),
      sharingView: null,
      active: null,
      nativeAvailable: true,
    }).detail,
    "Pair a computer to get started.",
  );
  const live = activeStatus({
    computers,
    sharingView: view({ phase: "sharing", peerFingerprint: WINDOWS, active: WINDOWS }),
    active: WINDOWS,
    nativeAvailable: true,
  });
  assert.equal(live.label, "Sharing input");
});
