import assert from "node:assert/strict";
import test from "node:test";

import { activeStatus, computerStatus, isLiveStatus, isSessionStatus } from "./computer-status.mjs";
import { initialComputers, normalizeComputers } from "./computers-model.mjs";

const WINDOWS = "b".repeat(64);
const MAC = "c".repeat(64);

const computer = { fingerprint: WINDOWS, name: "Office Windows PC", platform: "windows" };
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
  assert.equal(sends.detail, "Your keyboard and mouse reach Office Windows PC");
  assert.equal(isLiveStatus(sends), true);

  const receives = computerStatus(
    computer,
    view({ phase: "sharing", peerFingerprint: WINDOWS, active: WINDOWS, sharingRole: "receives" }),
    WINDOWS,
  );
  assert.equal(receives.detail, "Office Windows PC's keyboard and mouse reach this computer");
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
  assert.equal(editing.label, "Arranging displays");
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
  assert.equal(dropped.detail, "Reaching Office Windows PC.");

  const stopping = computerStatus(
    computer,
    view({ phase: "stopping", peerFingerprint: WINDOWS, active: WINDOWS }),
    WINDOWS,
  );
  assert.equal(stopping.label, "Stopping…");

  const failed = computerStatus(
    computer,
    view({ phase: "error", active: WINDOWS, message: "The other computer refused." }),
    WINDOWS,
  );
  assert.equal(failed.label, "Can't connect");
  assert.equal(failed.tone, "error");
  assert.equal(failed.detail, "The other computer refused.");
});

test("the computer in use with no worker yet is still on its way, never paused", () => {
  const dialing = computerStatus(computer, view({ phase: "off", active: WINDOWS }), WINDOWS);
  assert.equal(dialing.key, "connecting");
  assert.equal(dialing.detail, "Reaching Office Windows PC.");
});

// Only phase "error" with a message reads as a problem; an error with nothing confirmed yet still
// reads as ordinary dialing so a transient blip never looks like a failure.
test("an error with nothing confirmed yet reads as still connecting", () => {
  const unconfirmed = computerStatus(computer, view({ phase: "error", active: WINDOWS }), WINDOWS);
  assert.equal(unconfirmed.label, "Connecting…");
  assert.equal(unconfirmed.tone, "checking");
  assert.equal(isLiveStatus(unconfirmed), false);
});

// An unrecognized reply may hide a live worker, so it keeps the backend's own words in front of
// the user under a label that asks for a look without claiming the connection failed.
test("an unrecognized reply keeps its message, under a label that is not an alarm", () => {
  const message = "The connection state was not recognized. Stop, then connect again.";
  const unknown = computerStatus(
    computer,
    view({ phase: "unknown", active: WINDOWS, message }),
    WINDOWS,
  );
  assert.equal(unknown.label, "Needs attention");
  assert.equal(unknown.detail, message);
  assert.notEqual(unknown.tone, "error");
  assert.equal(isLiveStatus(unknown), false);
  // The peer is still named while the phase is unrecognized, and that changes nothing.
  assert.equal(
    computerStatus(
      computer,
      view({ phase: "unknown", peerFingerprint: WINDOWS, active: WINDOWS, message }),
      WINDOWS,
    ).label,
    "Needs attention",
  );
  // With nothing to say, it is simply still dialing.
  assert.equal(
    computerStatus(computer, view({ phase: "unknown", active: WINDOWS }), WINDOWS).label,
    "Connecting…",
  );
});

// Every message the backend sends with the link down, in its own words: a pause is the user's own
// doing, a reconnect or a started session is on its way back, and the rest is simply not connected.
test("each reason the link is down gets the label that reason deserves", () => {
  const cases = [
    ["Paused. Input is local.", "Paused", "paused"],
    ["Not connected. Input is local.", "Not connected", "paused"],
    ["Switching computers.", "Connecting…", "connecting"],
    ["The other computer left. Reconnecting.", "Connecting…", "connecting"],
    ["Arranging ended. Reconnecting.", "Connecting…", "connecting"],
    [
      "Arranging ended after 15 minutes without changes. Reconnecting.",
      "Connecting…",
      "connecting",
    ],
    ["Layout applied on both computers. Sharing is on.", "Connecting…", "connecting"],
    [
      "The displays changed since the layout was applied. Connecting to arrange them.",
      "Connecting…",
      "connecting",
    ],
  ];
  for (const [message, label, key] of cases) {
    const status = computerStatus(
      computer,
      view({ phase: "off", active: WINDOWS, message }),
      WINDOWS,
    );
    assert.equal(status.label, label, message);
    assert.equal(status.key, key, message);
    assert.equal(status.detail, message, message);
    assert.notEqual(status.tone, "error", message);
    assert.equal(isLiveStatus(status), false, message);
  }
});

// The status view is null until the first reply lands, and stays null when that call fails.
test("a computer renders before anything is known about the connection", () => {
  const waiting = computerStatus(computer, null, WINDOWS);
  assert.equal(waiting.label, "Connecting…");
  assert.equal(waiting.detail, "Reaching Office Windows PC.");
  assert.equal(isLiveStatus(waiting), false);
  assert.equal(isSessionStatus(waiting), false);

  const idle = computerStatus(computer, null, null);
  assert.equal(idle.key, "standby");
  assert.equal(idle.label, "Paired");
  assert.equal(computerStatus(other, undefined, WINDOWS).label, "Paired");
  assert.equal(computerStatus(null, null, null).label, "Connecting…");
});

test("the header pill names the computer in use, or why there is nothing to report", () => {
  const computers = normalizeComputers({
    computers: [
      { fingerprint: WINDOWS, name: "Office Windows PC", platform: "windows" },
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
