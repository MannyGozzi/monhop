import assert from "node:assert/strict";
import test from "node:test";

import { activeStatus, computerStatus, isLiveStatus, isSessionStatus } from "./computer-status.mjs";
import { initialComputers, normalizeComputers } from "./computers-model.mjs";

const WINDOWS = "b".repeat(64);
const MAC = "c".repeat(64);
const LOCAL_NAME = "This Mac";

const computer = { fingerprint: WINDOWS, name: "Office Windows PC", platform: "windows" };
const other = { fingerprint: MAC, name: "Studio Mac", platform: "macos" };

function view(patch = {}) {
  return {
    phase: "off",
    peerFingerprint: null,
    active: null,
    editing: false,
    control: null,
    lastFailure: "",
    message: "",
    ...patch,
  };
}

test("a computer nobody is using reads as paired, whatever the live connection is doing", () => {
  const live = view({ phase: "sharing", peerFingerprint: WINDOWS, active: WINDOWS });
  const standby = computerStatus(other, live, WINDOWS, LOCAL_NAME);
  assert.equal(standby.key, "standby");
  assert.equal(standby.label, "Paired");
  assert.equal(standby.tone, "neutral");
  assert.equal(isLiveStatus(standby), false);
});

test("both_directions_copy", () => {
  const both = computerStatus(
    computer,
    view({
      phase: "sharing",
      peerFingerprint: WINDOWS,
      active: WINDOWS,
      control: { localToPeer: true, peerToLocal: true },
    }),
    WINDOWS,
    LOCAL_NAME,
  );
  assert.equal(both.label, "Sharing");
  assert.equal(both.tone, "active");
  assert.equal(both.detail, "Either computer's keyboard and mouse can control the other");
  assert.equal(isLiveStatus(both), true);
  // No active record reads the same as both on, matching the default a fresh setup turns on.
  const noRecord = computerStatus(
    computer,
    view({ phase: "sharing", peerFingerprint: WINDOWS, active: WINDOWS, control: null }),
    WINDOWS,
    LOCAL_NAME,
  );
  assert.equal(noRecord.detail, both.detail);
});

test("one_direction_copy_names_controller_and_controlled", () => {
  const localToPeer = computerStatus(
    computer,
    view({
      phase: "sharing",
      peerFingerprint: WINDOWS,
      active: WINDOWS,
      control: { localToPeer: true, peerToLocal: false },
    }),
    WINDOWS,
    LOCAL_NAME,
  );
  assert.equal(localToPeer.detail, "This Mac's keyboard and mouse can control Office Windows PC");

  const peerToLocal = computerStatus(
    computer,
    view({
      phase: "sharing",
      peerFingerprint: WINDOWS,
      active: WINDOWS,
      control: { localToPeer: false, peerToLocal: true },
    }),
    WINDOWS,
    LOCAL_NAME,
  );
  assert.equal(peerToLocal.detail, "Office Windows PC's keyboard and mouse can control This Mac");
});

test("no_input_source_wording", async () => {
  const { readFile } = await import("node:fs/promises");
  const files = [
    "computer-status.mjs",
    "computer-card.mjs",
    "screen-home.mjs",
    "screen-displays.mjs",
  ];
  const sources = await Promise.all(
    files.map((file) => readFile(new URL(file, import.meta.url), "utf8")),
  );
  for (const [index, source] of sources.entries())
    assert.doesNotMatch(
      source,
      new RegExp(
        [
          "input computer",
          "keyboard computer",
          "input source",
          "source[S]ide",
          "sharing[R]ole",
        ].join("|"),
        "i",
      ),
      files[index],
    );
});

test("a session waiting out a silence reads as reconnecting, and stays live", () => {
  const held = computerStatus(
    computer,
    view({
      phase: "sharing",
      peerFingerprint: WINDOWS,
      active: WINDOWS,
      control: { localToPeer: true, peerToLocal: true },
      held: true,
    }),
    WINDOWS,
    LOCAL_NAME,
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
    localName: LOCAL_NAME,
  });
  assert.equal(live.label, "Sharing");
  assert.equal(live.detail, "Either computer's keyboard and mouse can control the other");
});

// --- the multi-computer `peers` shape ---------------------------------------------------

function peer(patch = {}) {
  return { fingerprint: WINDOWS, phase: "off", message: "", held: false, ...patch };
}

test("a computer named in view.peers reads its own phase, message and held flag from there", () => {
  const sharing = computerStatus(
    computer,
    view({ peers: [peer({ phase: "sharing" })] }),
    null,
    LOCAL_NAME,
  );
  assert.equal(sharing.key, "sharing");
  assert.equal(sharing.label, "Sharing");
  assert.equal(sharing.detail, "Either computer's keyboard and mouse can control the other");

  const held = computerStatus(
    computer,
    view({ peers: [peer({ phase: "sharing", held: true })] }),
    null,
  );
  assert.equal(held.key, "reconnecting");
  assert.match(held.detail, /Input stays on this computer/);

  const connected = computerStatus(computer, view({ peers: [peer({ phase: "connected" })] }), null);
  assert.equal(connected.key, "connected");
  assert.match(connected.detail, /Arrange the displays/);

  // Editing still comes from the shared view, not the peer entry: one editor for the whole group.
  const editing = computerStatus(
    computer,
    view({ peers: [peer({ phase: "connected" })], editing: true }),
    null,
  );
  assert.equal(editing.key, "editing");
  assert.match(editing.detail, /Sharing resumes after you apply/);

  const off = computerStatus(
    computer,
    view({ peers: [peer({ phase: "off", message: "Paused. Input is local." })] }),
    null,
  );
  assert.equal(off.key, "paused");
  assert.equal(off.label, "Paused");

  const stopping = computerStatus(computer, view({ peers: [peer({ phase: "stopping" })] }), null);
  assert.equal(stopping.label, "Stopping…");

  const failed = computerStatus(
    computer,
    view({ peers: [peer({ phase: "error", message: "The other computer refused." })] }),
    null,
  );
  assert.equal(failed.key, "error");
  assert.equal(failed.detail, "The other computer refused.");

  // No message yet: an error reads as still connecting, exactly like the legacy shape.
  const unconfirmed = computerStatus(computer, view({ peers: [peer({ phase: "error" })] }), null);
  assert.equal(unconfirmed.label, "Connecting…");
});

test("a member with no trust record yet reads as not paired, not as an error", () => {
  const notPaired = computerStatus(
    computer,
    view({ peers: [peer({ phase: "notPaired", message: "Not paired with this computer." })] }),
    null,
  );
  assert.equal(notPaired.key, "attention");
  assert.equal(notPaired.label, "Not paired");
  assert.equal(notPaired.detail, "Not paired with this computer.");
  assert.notEqual(notPaired.tone, "error");

  // With no message from the backend, a plain fallback still names the computer.
  const noMessage = computerStatus(computer, view({ peers: [peer({ phase: "notPaired" })] }), null);
  assert.equal(noMessage.detail, "Pair with Office Windows PC to share input with it.");
});

test("a computer not named in view.peers falls back to the legacy fields unchanged", () => {
  // peers present (even non-empty) but naming a different computer: this one is untouched by it.
  const untouched = computerStatus(
    computer,
    view({
      phase: "sharing",
      peerFingerprint: WINDOWS,
      active: WINDOWS,
      peers: [peer({ fingerprint: MAC, phase: "sharing" })],
    }),
    WINDOWS,
    LOCAL_NAME,
  );
  assert.equal(untouched.key, "sharing");

  // peers explicitly empty: identical to peers being absent altogether.
  const empty = computerStatus(
    computer,
    view({ phase: "sharing", peerFingerprint: WINDOWS, active: WINDOWS, peers: [] }),
    WINDOWS,
    LOCAL_NAME,
  );
  assert.equal(empty.key, "sharing");
});

// --- the header pill across several enabled computers -----------------------------------

function computersOf(...fingerprints) {
  return normalizeComputers({
    computers: fingerprints.map((fingerprint, index) => ({
      fingerprint,
      name: `Computer ${index + 1}`,
      platform: "windows",
    })),
  });
}

const THIRD = "d".repeat(64);

test("the header pill sums up sharing across every enabled computer", () => {
  const computers = computersOf(WINDOWS, MAC, THIRD);
  const allSharing = activeStatus({
    computers,
    sharingView: view({
      peers: [
        peer({ fingerprint: WINDOWS, phase: "sharing" }),
        peer({ fingerprint: MAC, phase: "sharing" }),
      ],
    }),
    enabled: [WINDOWS, MAC],
    nativeAvailable: true,
    localName: LOCAL_NAME,
  });
  assert.equal(allSharing.key, "sharing");
  assert.equal(allSharing.detail, "Sharing with 2 computers.");

  const mixed = activeStatus({
    computers,
    sharingView: view({
      peers: [
        peer({ fingerprint: WINDOWS, phase: "sharing" }),
        peer({ fingerprint: MAC, phase: "sharing" }),
        peer({ fingerprint: THIRD, phase: "connected" }),
      ],
    }),
    enabled: [WINDOWS, MAC, THIRD],
    nativeAvailable: true,
    localName: LOCAL_NAME,
  });
  assert.equal(mixed.key, "connecting");
  assert.equal(mixed.detail, "Connecting to 1 of 3 computers.");

  const noneYet = activeStatus({
    computers,
    sharingView: view({
      peers: [
        peer({ fingerprint: WINDOWS, phase: "connecting" }),
        peer({ fingerprint: MAC, phase: "connecting" }),
      ],
    }),
    enabled: [WINDOWS, MAC],
    nativeAvailable: true,
    localName: LOCAL_NAME,
  });
  assert.equal(noneYet.detail, "Connecting to 2 computers.");
});

test("a paused group reads as paused, whatever each computer's own phase is", () => {
  const paused = activeStatus({
    computers: computersOf(WINDOWS, MAC),
    sharingView: view({
      paused: true,
      peers: [
        peer({ fingerprint: WINDOWS, phase: "off" }),
        peer({ fingerprint: MAC, phase: "off" }),
      ],
    }),
    enabled: [WINDOWS, MAC],
    nativeAvailable: true,
    localName: LOCAL_NAME,
  });
  assert.equal(paused.key, "paused");
  assert.equal(paused.label, "Paused");
  assert.equal(paused.detail, "Paused for 2 computers.");
});

test("an unpaired member in the group is called out as something to connect, not a failure", () => {
  const status = activeStatus({
    computers: computersOf(WINDOWS, MAC),
    sharingView: view({
      peers: [
        peer({ fingerprint: WINDOWS, phase: "sharing" }),
        peer({ fingerprint: MAC, phase: "notPaired", message: "Not paired with this computer." }),
      ],
    }),
    enabled: [WINDOWS, MAC],
    nativeAvailable: true,
    localName: LOCAL_NAME,
  });
  assert.equal(status.key, "error");
  assert.equal(status.detail, "Can't connect to 1 of 2 computers.");
});

test("zero and one enabled computers keep today's exact wording even with an `enabled` list passed in", () => {
  const computers = computersOf(WINDOWS);
  const zero = activeStatus({
    computers,
    sharingView: view(),
    enabled: [],
    nativeAvailable: true,
    localName: LOCAL_NAME,
  });
  assert.equal(zero.label, "No computer");
  assert.equal(zero.detail, "Choose a computer to use.");

  const one = activeStatus({
    computers,
    sharingView: view({ phase: "sharing", peerFingerprint: WINDOWS, active: WINDOWS }),
    enabled: [WINDOWS],
    active: WINDOWS,
    nativeAvailable: true,
    localName: LOCAL_NAME,
  });
  assert.equal(one.label, "Sharing");
  assert.equal(one.detail, "Either computer's keyboard and mouse can control the other");
});
