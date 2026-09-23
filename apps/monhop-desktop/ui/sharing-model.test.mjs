import assert from "node:assert/strict";
import test from "node:test";

import { groupedPlacement, placementOffset } from "./arrangement-model.mjs";
import {
  MAX_CROSSINGS,
  applySharingView,
  arrangementForSharing,
  arrangementResetTarget,
  beginPending,
  canApplySetup,
  canEditLayout,
  canLoadArrangement,
  canResetArrangement,
  canSaveArrangement,
  displayNoticeCopy,
  failPending,
  hasAppliedCurrentLayout,
  hasAppliedLayout,
  displayUseChoices,
  hiddenDisplayIds,
  initialSharingState,
  initializeArrangement,
  isBusySharing,
  isConnected,
  isSessionActive,
  isEditingLayout,
  layoutForSave,
  layoutSignature,
  loadArrangement,
  normalizeArrangements,
  noticePresentation,
  normalizeDisplayNotice,
  normalizeSharingView,
  normalizeStoredLayout,
  placeArrangement,
  resetArrangement,
  sameSharingView,
  setDisplayInUse,
  setMonitorSide,
  sharedMonitorChoices,
  shouldPollSharing,
  setArrangement,
  setArrangements,
  settlePending,
  validateLayout,
} from "./sharing-model.mjs";

const localId = "18446744073709551615";
const peerId = "18446744073709551614";

const connectedView = {
  phase: "connected",
  revision: "42",
  localPlatform: "macos",
  peerPlatform: "windows",
  busy: false,
  sharingActive: false,
  message: "Connected.",
  link: { attempt: 1, since: "2026-09-12T03:00:00Z" },
  sync: { state: "idle", message: "" },
  localDisplays: [
    {
      id: localId,
      name: "Mac Built-in",
      origin: [0, 0],
      size: [1512, 982],
      scale: 2,
      primary: true,
    },
  ],
  peerDisplays: [
    {
      id: peerId,
      name: "Windows Desk",
      origin: [0, 0],
      size: [1920, 1080],
      scale: 1,
      primary: true,
    },
  ],
};

const offView = {
  ...connectedView,
  phase: "off",
  localDisplays: [],
  peerDisplays: [],
  message: "Not connected.",
};

function connected(patch = {}) {
  return applySharingView(initialSharingState(), { ...connectedView, ...patch });
}

function savedLayout() {
  return {
    links: [
      {
        fromDisplay: peerId,
        fromEdge: "right",
        fromSpan: [0, 1],
        toDisplay: localId,
        toEdge: "left",
        toSpan: [0, 1],
        hysteresis: 1,
      },
      {
        fromDisplay: localId,
        fromEdge: "left",
        fromSpan: [0, 1],
        toDisplay: peerId,
        toEdge: "right",
        toSpan: [0, 1],
        hysteresis: 1,
      },
    ],
  };
}

const groupsOf = (state) => arrangementForSharing(state).groups;
// Drops the peer computer's whole block at an offset from the local computer's block.
const place = (state, offset) => setArrangement(state, groupedPlacement(groupsOf(state), offset));
const offsetOf = (state) =>
  placementOffset(groupsOf(state), arrangementForSharing(state).placement);

test("the view names which computer is live and which one is in use, in one case", () => {
  const view = normalizeSharingView({
    ...connectedView,
    peerFingerprint: "B".repeat(64),
    active: "B".repeat(64),
    editing: true,
  });
  assert.equal(view.peerFingerprint, "b".repeat(64));
  assert.equal(view.active, "b".repeat(64));
  assert.equal(view.editing, true);
  const idle = normalizeSharingView(offView);
  assert.equal(idle.peerFingerprint, null);
  assert.equal(idle.active, null);
  assert.equal(idle.editing, false);
  assert.equal(normalizeSharingView({ ...offView, active: "short" }).active, null);
  assert.equal(normalizeSharingView({ garbage: true }).peerFingerprint, null);
  assert.equal(isEditingLayout(applySharingView(initialSharingState(), connectedView)), false);
  assert.equal(
    isEditingLayout(applySharingView(initialSharingState(), { ...connectedView, editing: true })),
    true,
  );
});

test("native link views reject malformed IDs, coordinates, phases, and empty connected topologies", () => {
  for (const patch of [
    { localDisplays: [{ ...connectedView.localDisplays[0], id: "1e3" }] },
    { localDisplays: [{ ...connectedView.localDisplays[0], id: "01" }] },
    { localDisplays: [{ ...connectedView.localDisplays[0], id: "18446744073709551616" }] },
    { peerDisplays: [{ ...connectedView.peerDisplays[0], origin: [0, Infinity] }] },
    { peerDisplays: [{ ...connectedView.peerDisplays[0], size: [0, 1080] }] },
    { revision: "revision-42" },
    { revision: "18446744073709551616" },
    { peerDisplays: [{ ...connectedView.peerDisplays[0], id: localId }] },
    { phase: "ready" },
    { phase: "connected", localDisplays: [] },
    { busy: "yes" },
  ]) {
    assert.equal(normalizeSharingView({ ...connectedView, ...patch }).recognized, false);
  }
  const view = normalizeSharingView({
    ...connectedView,
    sync: { state: "weird" },
    link: { attempt: -1 },
  });
  assert.equal(view.recognized, true);
  assert.equal(view.sync.state, "idle");
  assert.equal(view.link.attempt, 0);
  assert.equal(normalizeSharingView(offView).recognized, true);
});

test("control names which directions are allowed and preserves the backend sync window", () => {
  const view = normalizeSharingView({
    ...offView,
    phase: "sharing",
    sharingActive: true,
    control: { localToPeer: true, peerToLocal: false, syncing: true },
  });
  assert.equal(view.recognized, true);
  assert.deepEqual(view.control, { localToPeer: true, peerToLocal: false, syncing: true });
  const bothOff = normalizeSharingView({
    ...offView,
    control: { localToPeer: false, peerToLocal: false, syncing: false },
  });
  assert.equal(bothOff.control, null);
  const malformed = normalizeSharingView({
    ...offView,
    control: { localToPeer: "yes", peerToLocal: true, syncing: false },
  });
  assert.equal(malformed.control, null);
  // No active record yet: control is null, not a guess.
  assert.equal(normalizeSharingView(offView).control, null);
  assert.equal(normalizeSharingView(connectedView).control, null);
  assert.deepEqual(
    normalizeSharingView({ ...offView, control: { localToPeer: true, peerToLocal: true } }).control,
    { localToPeer: true, peerToLocal: true, syncing: false },
  );
});

test("the display notice normalizes to a known kind or drops to null", () => {
  assert.deepEqual(normalizeDisplayNotice({ kind: "waiting" }), { kind: "waiting" });
  assert.deepEqual(normalizeDisplayNotice({ kind: "continued" }), { kind: "continued" });
  assert.deepEqual(normalizeDisplayNotice({ kind: "updating" }), { kind: "updating" });
  assert.deepEqual(normalizeDisplayNotice({ kind: "peerDeciding" }), { kind: "peerDeciding" });
  for (const value of [null, undefined, {}, { kind: "confused" }, "waiting", 1, []])
    assert.equal(normalizeDisplayNotice(value), null);
  assert.equal(normalizeSharingView(connectedView).displayNotice, null);
  assert.deepEqual(
    normalizeSharingView({ ...connectedView, displayNotice: { kind: "waiting" } }).displayNotice,
    { kind: "waiting" },
  );
  assert.equal(
    normalizeSharingView({ ...connectedView, displayNotice: { kind: "nope" } }).displayNotice,
    null,
  );
  const notified = applySharingView(initialSharingState(), {
    ...connectedView,
    displayNotice: { kind: "continued" },
  });
  assert.deepEqual(notified.view.displayNotice, { kind: "continued" });
});

test("a display change the user must settle gets the banner; one MonHop is settling gets a line", () => {
  assert.equal(noticePresentation("waiting"), "banner");
  assert.equal(noticePresentation("continued"), "banner");
  assert.equal(noticePresentation("updating"), "inline");
  assert.equal(noticePresentation("peerDeciding"), "inline");
  assert.equal(noticePresentation("bogus"), null);

  const waiting = displayNoticeCopy("waiting", "Office Windows PC");
  assert.equal(waiting.presentation, "banner");
  assert.equal(waiting.title, "Your displays changed");
  assert.equal(waiting.body, "No saved layout fits. Arrange the displays to start sharing.");
  assert.equal(waiting.primaryLabel, "Arrange displays");
  assert.equal(waiting.secondaryLabel, "Dismiss");

  const continued = displayNoticeCopy("continued", "Office Windows PC");
  assert.equal(continued.presentation, "banner");
  assert.equal(continued.title, "Your displays changed");
  assert.equal(
    continued.body,
    "Sharing continues with a layout adapted to them. Arrange the displays if you want something different.",
  );
  assert.equal(continued.primaryLabel, "Arrange displays");
  assert.equal(continued.secondaryLabel, "Keep going");

  const updating = displayNoticeCopy("updating", "Office Windows PC");
  assert.equal(updating.presentation, "inline");
  assert.equal(updating.body, "Updating the layout…");
  assert.equal(updating.title, null);
  assert.equal(updating.primaryLabel, null);
  assert.equal(updating.secondaryLabel, null);

  const peerDeciding = displayNoticeCopy("peerDeciding", "Office Windows PC");
  assert.equal(peerDeciding.presentation, "inline");
  assert.equal(peerDeciding.body, "Office Windows PC is choosing the layout.");
  assert.equal(peerDeciding.primaryLabel, null);

  assert.equal(displayNoticeCopy("bogus", "Office Windows PC"), null);
});

test("the link phases decide what the user can do", () => {
  let state = initialSharingState();
  assert.equal(canEditLayout(state, "en0"), true);
  assert.equal(canEditLayout(state, ""), false);
  state = beginPending(state, "edit");
  assert.equal(canEditLayout(state, "en0"), false);
  assert.equal(isBusySharing(state), true);
  state = applySharingView(settlePending(state, state.generation), {
    ...offView,
    phase: "connecting",
    link: { attempt: 2, since: "" },
  });
  assert.equal(isSessionActive(state), true);
  assert.equal(isConnected(state), false);
  // A dialing link is the supervisor's; the editor may open on it (the backend adopts the link).
  assert.equal(canEditLayout(state, "en0"), true);
  assert.equal(isBusySharing(state), false);
  state = applySharingView(state, connectedView);
  assert.equal(isConnected(state), true);
  state = applySharingView(state, {
    ...connectedView,
    phase: "reconnecting",
    localDisplays: [],
    peerDisplays: [],
  });
  assert.equal(isSessionActive(state), true);
  assert.equal(isConnected(state), false);
  state = applySharingView(state, offView);
  assert.equal(isSessionActive(state), false);
  assert.equal(canEditLayout(state, "en0"), true);
});

test("late or failed replies never overwrite a newer command", () => {
  let state = beginPending(initialSharingState(), "edit");
  const stale = state.generation;
  state = beginPending(state, "active");
  assert.equal(settlePending(state, stale), state);
  assert.equal(failPending(state, stale, "old failure"), state);
  state = failPending(state, state.generation, "That did not finish.");
  assert.equal(state.pending, null);
  assert.equal(state.message, "That did not finish.");
});

test("an unrecognized reply marks the connection unknown and reports it plainly", () => {
  let state = connected();
  state = applySharingView(state, { garbage: true });
  assert.equal(state.view.phase, "unknown");
  assert.equal(shouldPollSharing(state), true);
  assert.match(state.message, /not recognized/i);
});

test("polling the same applied layout keeps the local draft", () => {
  let state = initializeArrangement(connected());
  const appliedView = {
    ...connectedView,
    sync: { state: "applied", message: "" },
    synchronizedLayout: layoutForSave(state),
  };
  state = applySharingView(state, appliedView);
  state = place(state, [1512, 300]);
  const edited = state.layout;
  state = applySharingView(state, appliedView);
  assert.deepEqual(state.layout, edited);
});

test("every seam emits forward and reverse links", () => {
  const state = initializeArrangement(connected());
  const result = validateLayout(state);
  assert.equal(result.ok, true);
  assert.equal(result.layout.source, undefined);
  assert.deepEqual(
    result.layout.links.map((link) => [
      link.fromDisplay,
      link.fromEdge,
      link.toDisplay,
      link.toEdge,
      link.hysteresis,
    ]),
    [
      [localId, "right", peerId, "left", 1],
      [peerId, "left", localId, "right", 1],
    ],
  );
  assert.deepEqual(result.layout.links[0].fromSpan, [0, 1]);
  assert.deepEqual(result.layout.links[0].toSpan, [49 / 1080, 1031 / 1080]);
  assert.deepEqual(result.layout.links[1].fromSpan, [49 / 1080, 1031 / 1080]);
  assert.deepEqual(result.layout.links[1].toSpan, [0, 1]);
});

test("layouts reject duplicate directed edges, foreign displays, identical IDs, and too many crossings", () => {
  const state = connected();
  const crossing = {
    id: "one",
    fromDisplay: localId,
    fromEdge: "right",
    toDisplay: peerId,
    toEdge: "left",
  };
  for (const layout of [
    { crossings: [crossing, { ...crossing, id: "two" }] },
    { crossings: [{ ...crossing, fromDisplay: "999" }] },
    { crossings: [{ ...crossing, toDisplay: localId }] },
    {
      crossings: Array.from({ length: MAX_CROSSINGS + 1 }, (_, index) => ({
        ...crossing,
        id: String(index),
      })),
    },
  ]) {
    assert.equal(validateLayout(state, layout).ok, false);
  }
  assert.equal(validateLayout(applySharingView(state, offView)).ok, false);
});

test("a crossing on the peer's own native seam is rejected", () => {
  const lg = { id: "301", name: "LG", origin: [0, 0], size: [2560, 1440], scale: 1, primary: true };
  const dell = {
    id: "302",
    name: "Dell",
    origin: [-2560, 213],
    size: [2560, 1440],
    scale: 1,
    primary: false,
  };
  const mac = { ...connectedView.localDisplays[0], monitor: "10ac-d0e5-30305455" };
  const layout = {
    crossings: [
      { id: "one", fromDisplay: mac.id, fromEdge: "right", toDisplay: lg.id, toEdge: "left" },
    ],
  };
  const blocked = validateLayout(
    connected({ peerDisplays: [lg, dell], localDisplays: [mac] }),
    layout,
  );
  assert.equal(blocked.ok, false);
  assert.match(blocked.message, /left edge of LG already leads to Dell/);
  // Marking the two as the same physical monitor removes the peer's own seam entirely.
  const shared = connected({
    peerDisplays: [lg, { ...dell, monitor: mac.monitor }],
    localDisplays: [mac],
  });
  assert.equal(validateLayout(shared, layout).ok, true);
});

test("a crossing on the local computer's own native seam is rejected too", () => {
  // The Mac's own OS stacks these two displays: the top one directly above the bottom one, touching.
  const macTop = {
    id: "401",
    name: "Mac Top",
    origin: [0, 0],
    size: [1512, 982],
    scale: 2,
    primary: true,
  };
  const macBottom = {
    id: "402",
    name: "Mac Bottom",
    origin: [0, 982],
    size: [1512, 982],
    scale: 2,
    primary: false,
  };
  const layout = {
    crossings: [
      { id: "one", fromDisplay: macBottom.id, fromEdge: "top", toDisplay: peerId, toEdge: "bottom" },
    ],
  };
  const blocked = validateLayout(connected({ localDisplays: [macTop, macBottom] }), layout);
  assert.equal(blocked.ok, false);
  assert.match(blocked.message, /top edge of Mac Bottom already leads to Mac Top/);
});

test("apply needs a connected link and no sync in flight", () => {
  let state = initializeArrangement(connected());
  assert.equal(canApplySetup(state), true);
  assert.equal(
    canApplySetup(
      applySharingView(state, { ...connectedView, sync: { state: "sending", message: "" } }),
    ),
    false,
  );
  assert.equal(canApplySetup(beginPending(state, "apply")), false);
  assert.equal(canApplySetup(applySharingView(state, { ...connectedView, busy: true })), false);
});

test("an applied sync becomes the current layout and stays applied until it changes", () => {
  let state = initializeArrangement(connected());
  const native = layoutForSave(state);
  state = applySharingView(state, {
    ...connectedView,
    sync: { state: "applied", message: "Layout applied on both computers." },
    synchronizedLayout: native,
  });
  assert.equal(hasAppliedCurrentLayout(state), true);
  assert.equal(layoutSignature(layoutForSave(state)), layoutSignature(native));
  state = place(state, [1512, 300]);
  assert.equal(hasAppliedCurrentLayout(state), false);
});

test("a rejected sync leaves the draft untouched", () => {
  let state = initializeArrangement(connected());
  const draft = state.layout;
  state = applySharingView(state, {
    ...connectedView,
    sync: { state: "rejected", message: "The displays changed. Arrange again." },
  });
  assert.deepEqual(state.layout, draft);
  assert.equal(hasAppliedCurrentLayout(state), false);
  // The rejection belongs to the sync it describes; the alert channel is for what only the UI knows.
  assert.equal(state.view.sync.message, "The displays changed. Arrange again.");
  assert.equal(state.message, "");
});

test("a topology change invalidates a draft that no longer fits and asks to arrange again", () => {
  let state = initializeArrangement(connected());
  state = applySharingView(state, {
    ...connectedView,
    revision: "43",
    peerDisplays: [{ ...connectedView.peerDisplays[0], id: "77" }],
  });
  assert.equal(state.layout.crossings.length, 0);
  assert.match(state.message, /displays changed/i);
  let same = initializeArrangement(connected());
  same = applySharingView(same, { ...connectedView, revision: "43" });
  assert.equal(same.layout.crossings.length, 1);
});

test("losing the link keeps the draft but nothing can be applied until it returns", () => {
  let state = initializeArrangement(connected());
  state = applySharingView(state, offView);
  assert.equal(state.layout.crossings.length, 1);
  assert.equal(canApplySetup(state), false);
  assert.equal(layoutForSave(state), null);
  // Dialing again shows no displays yet, which must not read as a draft that no longer fits.
  state = applySharingView(state, {
    ...offView,
    phase: "connecting",
    link: { attempt: 1, since: "" },
  });
  assert.equal(state.layout.crossings.length, 1);
  state = applySharingView(state, connectedView);
  assert.equal(state.layout.crossings.length, 1);
  assert.equal(validateLayout(state).ok, true);
});

test("an applied layout carries only links and positions, no source display and no mode, and restores the exact pose", () => {
  let state = initializeArrangement(connected());
  assert.equal(arrangementForSharing(state).connected, true);
  state = place(state, [1512, 0]);
  const placement = arrangementForSharing(state).placement;
  assert.equal(canApplySetup(state), true);
  const native = layoutForSave(state);
  assert.deepEqual(Object.keys(native).toSorted(), ["arrangement", "links"]);
  assert.deepEqual(Object.keys(native.arrangement).toSorted(), ["hidden", "positions"]);
  assert.deepEqual(native.links[0].toSpan, [0, 982 / 1080]);
  assert.deepEqual(native.arrangement, {
    positions: [
      { display: peerId, x: 1512, y: 0 },
      { display: localId, x: 0, y: 0 },
    ],
    hidden: [],
  });
  // The other computer applies the same layout: its editor shows the identical picture.
  let peer = connected();
  peer = applySharingView(peer, {
    ...connectedView,
    sync: { state: "applied", message: "" },
    synchronizedLayout: native,
  });
  assert.deepEqual(arrangementForSharing(peer).placement, placement);
  assert.equal(hasAppliedCurrentLayout(peer), true);
  state = place(state, [1517, 0]);
  assert.deepEqual(offsetOf(state), [1512, 0]);
  assert.equal(arrangementForSharing(state).connected, true);
});

test("named arrangements list only well-formed entries and load only while they still fit", () => {
  let state = place(initializeArrangement(connected()), [1512, 300]);
  const native = layoutForSave(state);
  const listed = normalizeArrangements([
    { name: "  Desk  ", crossings: 1, layout: native },
    { name: "Desk", crossings: 1, layout: native },
    { name: "Couch", crossings: 2, layout: null },
    { name: "", crossings: 1, layout: native },
    { name: "x".repeat(65), crossings: 1, layout: native },
  ]);
  assert.deepEqual(
    listed.map((entry) => [entry.name, entry.crossings, Boolean(entry.layout)]),
    [
      ["Desk", 1, true],
      ["Couch", 2, false],
    ],
  );
  assert.deepEqual(normalizeArrangements("nope"), []);
  state = setArrangements(place(state, [1512, 49]), listed);
  assert.equal(canLoadArrangement(state, "Desk"), true);
  assert.equal(canLoadArrangement(state, "Couch"), false);
  assert.equal(canLoadArrangement(state, "Nope"), false);
  const loaded = loadArrangement(state, "Desk");
  assert.deepEqual(offsetOf(loaded), [1512, 300]);
  assert.equal(loaded.message, "");
  assert.equal(canSaveArrangement(state, "Desk"), true);
  assert.equal(canSaveArrangement(state, "   "), false);
  assert.equal(canSaveArrangement(applySharingView(state, offView), "Desk"), false);
  assert.deepEqual(applySharingView(state, offView).arrangements, []);
});

test("an arrangement is automatic only when the native reply says so exactly", () => {
  const native = layoutForSave(place(initializeArrangement(connected()), [1512, 300]));
  const [remembered, named, defaulted] = normalizeArrangements([
    { name: "Desk", crossings: 1, layout: native, automatic: true },
    { name: "Couch", crossings: 2, layout: native, automatic: false },
    { name: "Loft", crossings: 2, layout: native, automatic: "yes" },
  ]);
  assert.equal(remembered.automatic, true);
  assert.equal(named.automatic, false);
  assert.equal(defaulted.automatic, false);
});

test("an arrangement fits only when the native reply says so exactly", () => {
  const native = layoutForSave(place(initializeArrangement(connected()), [1512, 300]));
  const [fits, doesNotFit, defaulted] = normalizeArrangements([
    { name: "Desk", crossings: 1, layout: native, fits: true },
    { name: "Couch", crossings: 2, layout: null, fits: false },
    { name: "Loft", crossings: 2, layout: null },
  ]);
  assert.equal(fits.fits, true);
  assert.equal(doesNotFit.fits, false);
  assert.equal(defaulted.fits, false);
});

test("stored layouts carry their arrangement only when it is well-formed", () => {
  const layout = savedLayout();
  assert.equal(normalizeStoredLayout(layout).arrangement, undefined);
  const positions = [
    { display: peerId, x: 0, y: 0 },
    { display: localId, x: 1512, y: 0.5 },
  ];
  assert.deepEqual(
    normalizeStoredLayout({ ...layout, arrangement: { positions } }).arrangement,
    { positions },
  );
  assert.deepEqual(
    normalizeStoredLayout({ ...layout, arrangement: { positions, hidden: ["7"] } }).arrangement,
    { positions, hidden: ["7"] },
  );
  for (const arrangement of [
    { positions, hidden: [peerId] },
    { positions, hidden: ["7", "7"] },
    { positions, hidden: "none" },
    { positions: [...positions, { display: peerId, x: 1, y: 1 }] },
    { positions: [{ display: "01", x: 0, y: 0 }] },
    { positions: [{ display: peerId, x: Infinity, y: 0 }] },
    { positions: "everywhere" },
  ]) {
    assert.equal(normalizeStoredLayout({ ...layout, arrangement }), null);
  }
});

test("two partial contacts may share an edge endpoint but not overlap their interiors", () => {
  let state = connected({
    localDisplays: [
      { ...connectedView.localDisplays[0], size: [100, 100] },
      {
        ...connectedView.localDisplays[0],
        id: "77",
        origin: [0, 100],
        size: [100, 100],
        primary: false,
      },
    ],
    peerDisplays: [{ ...connectedView.peerDisplays[0], size: [100, 200] }],
  });
  state = place(initializeArrangement(state), [100, 0]);
  assert.equal(validateLayout(state).ok, true);
  assert.equal(layoutForSave(state).links.length, 4);
  assert.deepEqual(
    layoutForSave(state)
      .links.filter((l) => l.fromDisplay === peerId)
      .map((l) => l.fromSpan),
    [
      [0, 0.5],
      [0.5, 1],
    ],
  );
  const tampered = structuredClone(state);
  tampered.layout.crossings[1].fromSpan = [0.4, 1];
  assert.equal(validateLayout(tampered).ok, false);
  const off = applySharingView(state, offView);
  assert.equal(place(off, [100, 0]), off);
});

test("a running sharing session leaves the screen usable and still accepts a new setup link", () => {
  const live = applySharingView(initialSharingState(), {
    ...offView,
    phase: "sharing",
    sharingActive: true,
  });
  assert.equal(isSessionActive(live), true);
  assert.equal(isBusySharing(live), false);
  // Opening the editor stops the session natively, so changing a layout never needs it paused first.
  assert.equal(canEditLayout(live, "en0"), true);
  const starting = applySharingView(live, {
    ...offView,
    phase: "starting",
    link: { attempt: 3, since: "" },
    message: "Reaching the other computer.",
  });
  // The supervisor dials by itself, so a dialing session never freezes the rest of the screen.
  assert.equal(isBusySharing(starting), false);
  assert.equal(canEditLayout(starting, "en0"), true);
  const linking = applySharingView(live, { ...offView, phase: "connecting", busy: true });
  assert.equal(isBusySharing(linking), false);
  assert.equal(canEditLayout(linking, "en0"), true);
  const stopping = applySharingView(live, { ...offView, phase: "stopping", busy: true });
  assert.equal(isBusySharing(stopping), true);
  assert.equal(canEditLayout(stopping, "en0"), false);
});

test("status polling follows the computer in use, not the screen", () => {
  assert.equal(shouldPollSharing(initialSharingState()), false);
  const off = applySharingView(initialSharingState(), offView);
  assert.equal(shouldPollSharing(off), false);
  // The computer list learns of a new pairing before the sharing view is read again.
  assert.equal(shouldPollSharing(off, "b".repeat(64)), true);
  assert.equal(
    shouldPollSharing(applySharingView(off, { ...offView, active: "b".repeat(64) })),
    true,
  );
  assert.equal(shouldPollSharing(applySharingView(off, { ...offView, phase: "starting" })), true);
  assert.equal(shouldPollSharing(applySharingView(off, { garbage: true })), true);
  // Nothing in use and nothing running: an error is the last word until the user acts.
  assert.equal(
    shouldPollSharing(
      applySharingView(off, { ...offView, phase: "error", message: "It stopped." }),
    ),
    false,
  );
});

test("the applied receipt outlives the link that carried it and a later edit clears it", () => {
  let state = initializeArrangement(connected());
  assert.equal(hasAppliedLayout(state), false);
  state = applySharingView(state, {
    ...connectedView,
    sync: { state: "applied", message: "" },
    synchronizedLayout: layoutForSave(state),
  });
  assert.equal(hasAppliedLayout(state), true);
  // Applying starts sharing and closes the link; the success state has to remain on screen.
  const shared = applySharingView(state, {
    ...offView,
    phase: "sharing",
    sharingActive: true,
  });
  assert.equal(hasAppliedLayout(shared), true);
  assert.equal(hasAppliedLayout(place(state, [1512, 300])), false);
  const reconnected = applySharingView(shared, connectedView);
  assert.equal(reconnected.syncApplied, false);
  assert.equal(hasAppliedLayout(reconnected), false);
});

test("the applied receipt survives the link closing itself, even if no poll saw it connected", () => {
  const state = initializeArrangement(connected());
  const appliedSync = {
    state: "applied",
    message: "Layout applied on both computers. Sharing is on.",
  };
  const stopping = applySharingView(state, {
    ...offView,
    phase: "stopping",
    busy: true,
    sync: appliedSync,
  });
  assert.equal(stopping.syncApplied, true);
  assert.equal(hasAppliedLayout(stopping), true);
  const off = applySharingView(stopping, { ...offView, phase: "off", sync: appliedSync });
  assert.equal(hasAppliedLayout(off), true);
  // The supervisor takes over: still applied, and still not an error.
  assert.equal(
    hasAppliedLayout(
      applySharingView(off, {
        ...offView,
        phase: "starting",
        message: "Connecting to the other computer for sharing.",
      }),
    ),
    true,
  );
});

test("an overlapping drop snaps to a touching one, and an unusable drop is never stored", () => {
  let state = initializeArrangement(connected());
  // The requested offset overlaps the local group; it snaps to the nearest offset that only touches.
  const resolved = place(state, [960, 100]);
  assert.notEqual(resolved, state);
  const geometry = arrangementForSharing(resolved);
  assert.equal(geometry.valid, true);
  assert.equal(geometry.connected, true);
  assert.deepEqual(offsetOf(resolved), [1512, 100]);
  assert.equal(validateLayout(resolved).ok, true);
  assert.equal(place(state, [NaN, 0]), state);
  assert.equal(place(state, [Infinity, 0]), state);
  // A placement missing one computer's positions is not a placement at all.
  assert.equal(setArrangement(state, { positions: { [peerId]: [0, 0] } }), state);
});

test("reset goes back to the applied arrangement, and is unavailable while nothing differs", () => {
  let state = initializeArrangement(connected());
  assert.equal(arrangementResetTarget(state).origin, "default");
  assert.equal(canResetArrangement(state), false);
  state = place(state, [1512, 300]);
  assert.equal(canResetArrangement(state), true);
  assert.deepEqual(
    placementOffset(groupsOf(state), arrangementResetTarget(state).placement),
    [1512, -49],
  );
  state = setArrangement(state, arrangementResetTarget(state).placement);
  assert.equal(canResetArrangement(state), false);

  state = place(state, [1512, 300]);
  state = applySharingView(state, {
    ...connectedView,
    sync: { state: "applied", message: "" },
    synchronizedLayout: layoutForSave(state),
  });
  assert.equal(canResetArrangement(state), false);
  state = place(state, [1512, 0]);
  const target = arrangementResetTarget(state);
  assert.equal(target.origin, "applied");
  assert.deepEqual(placementOffset(groupsOf(state), target.placement), [1512, 300]);
  assert.equal(canResetArrangement(state), true);
  assert.deepEqual(offsetOf(setArrangement(state, target.placement)), [1512, 300]);
});

const sharedKey = "10ac-4123-0000abcd";
const macTwin = {
  id: "77",
  name: "Desk Monitor",
  origin: [1512, 0],
  size: [2560, 1440],
  scale: 1,
  primary: false,
  monitor: sharedKey,
};
const windowsTwin = {
  id: "88",
  name: "Desk Monitor",
  origin: [1920, 0],
  size: [2560, 1440],
  scale: 1,
  primary: false,
  monitor: sharedKey,
};
const twinView = {
  localDisplays: [connectedView.localDisplays[0], macTwin],
  peerDisplays: [connectedView.peerDisplays[0], windowsTwin],
};

test("a monitor cabled to both computers is arranged once and either computer can be marked as showing on it", () => {
  let state = initializeArrangement(connected(twinView));
  // This computer keeps its own copy by default; the peer's copy leaves the picture.
  assert.deepEqual(hiddenDisplayIds(state), ["88"]);
  assert.deepEqual(sharedMonitorChoices(state), [
    { monitor: sharedKey, name: "Desk Monitor", side: "local", canSwap: true },
  ]);
  assert.equal(arrangementForSharing(state).connected, true);
  const native = layoutForSave(state);
  assert.deepEqual(native.arrangement.hidden, ["88"]);
  assert.ok(native.links.every((link) => link.fromDisplay !== "88" && link.toDisplay !== "88"));
  // Mark the peer as the computer showing on it: the tile changes sides and stays connected.
  const marked = setMonitorSide(state, sharedKey, "peer");
  assert.deepEqual(hiddenDisplayIds(marked), ["77"]);
  assert.deepEqual(sharedMonitorChoices(marked), [
    { monitor: sharedKey, name: "Desk Monitor", side: "peer", canSwap: true },
  ]);
  assert.equal(arrangementForSharing(marked).connected, true);
  assert.equal(setMonitorSide(marked, sharedKey, "peer"), marked);
  assert.equal(setMonitorSide(marked, "0000-0000-00000000", "local"), marked);
  // The other computer adopts the applied layout with the same copy hidden.
  const appliedLayout = layoutForSave(marked);
  assert.deepEqual(appliedLayout.arrangement.hidden, ["77"]);
  let peer = applySharingView(connected(twinView), {
    ...connectedView,
    ...twinView,
    sync: { state: "applied", message: "" },
    synchronizedLayout: appliedLayout,
  });
  assert.deepEqual(hiddenDisplayIds(peer), ["77"]);
  assert.equal(hasAppliedCurrentLayout(peer), true);
  // Reset goes back to the applied picture, including which computer shows on the monitor.
  const flipped = setMonitorSide(peer, sharedKey, "local");
  assert.deepEqual(hiddenDisplayIds(flipped), ["88"]);
  assert.equal(hasAppliedCurrentLayout(flipped), false);
  assert.equal(canResetArrangement(flipped), true);
  const reset = resetArrangement(flipped);
  assert.deepEqual(hiddenDisplayIds(reset), ["77"]);
  assert.equal(hasAppliedCurrentLayout(reset), true);
  assert.equal(canResetArrangement(reset), false);
});

test("a computer with only the shared monitor cannot give it away", () => {
  const lone = { ...windowsTwin, primary: true };
  let state = initializeArrangement(
    connected({ localDisplays: twinView.localDisplays, peerDisplays: [lone] }),
  );
  assert.deepEqual(hiddenDisplayIds(state), ["77"]);
  assert.deepEqual(sharedMonitorChoices(state), [
    { monitor: sharedKey, name: "Desk Monitor", side: "peer", canSwap: false },
  ]);
  const refused = setMonitorSide(state, sharedKey, "local");
  assert.deepEqual(hiddenDisplayIds(refused), ["77"]);
  assert.match(refused.message, /only display/);
});

test("any display can be marked not in use, and the last one a computer has stays", () => {
  let state = initializeArrangement(connected(twinView));
  assert.deepEqual(hiddenDisplayIds(state), ["88"]);
  const choice = (id, side, patch) => ({
    id,
    side,
    name: id === peerId ? "Windows Desk" : id === localId ? "Mac Built-in" : "Desk Monitor",
    size: id === peerId ? [1920, 1080] : id === localId ? [1512, 982] : [2560, 1440],
    primary: id === peerId || id === localId,
    cabledToBoth: id === "77" || id === "88",
    inUse: true,
    canLeave: true,
    ...patch,
  });
  assert.deepEqual(displayUseChoices(state), {
    local: [choice(localId, "local"), choice("77", "local")],
    peer: [choice(peerId, "peer", { canLeave: false }), choice("88", "peer", { inUse: false })],
  });
  // Both copies of the shared monitor can be in use at once; the picture stays connected.
  const both = setDisplayInUse(state, "88", true);
  assert.deepEqual(hiddenDisplayIds(both), []);
  assert.equal(arrangementForSharing(both).connected, true);
  assert.deepEqual(sharedMonitorChoices(both), []);
  assert.deepEqual(layoutForSave(both).arrangement.hidden, []);
  // Turning the peer's own display off while both shared copies show still protects its last one.
  const noPeerMain = setDisplayInUse(both, peerId, false);
  assert.deepEqual(hiddenDisplayIds(noPeerMain), [peerId]);
  assert.equal(arrangementForSharing(noPeerMain).connected, true);
  const refusedPeer = setDisplayInUse(noPeerMain, "88", false);
  assert.deepEqual(hiddenDisplayIds(refusedPeer), [peerId]);
  assert.match(refusedPeer.message, /only display/);
  // The same protection holds for the local computer, whichever display was hidden first.
  const hideTwin = setDisplayInUse(state, "77", false);
  assert.deepEqual(hiddenDisplayIds(hideTwin), ["77", "88"]);
  const refusedLocal = setDisplayInUse(hideTwin, localId, false);
  assert.deepEqual(hiddenDisplayIds(refusedLocal), ["77", "88"]);
  assert.match(refusedLocal.message, /only display/);
  // Nothing changes for a mark that already holds or a display nobody reports.
  assert.equal(setDisplayInUse(state, "88", false), state);
  assert.equal(setDisplayInUse(state, "77", true), state);
  assert.equal(setDisplayInUse(state, "9", false), state);
  // The other computer adopts an applied layout that shows every display, and reset keeps it that way.
  const everyDisplay = layoutForSave(both);
  const peer = applySharingView(connected(twinView), {
    ...connectedView,
    ...twinView,
    sync: { state: "applied", message: "" },
    synchronizedLayout: everyDisplay,
  });
  assert.deepEqual(hiddenDisplayIds(peer), []);
  assert.equal(hasAppliedCurrentLayout(peer), true);
  const hiddenAgain = setDisplayInUse(peer, "88", false);
  assert.deepEqual(hiddenDisplayIds(hiddenAgain), ["88"]);
  assert.equal(canResetArrangement(hiddenAgain), true);
  assert.deepEqual(hiddenDisplayIds(resetArrangement(hiddenAgain)), []);
});

test("sameSharingView compares nested arrays and null fields structurally, not by reference", () => {
  const a = normalizeSharingView(connectedView);
  const b = normalizeSharingView({
    ...connectedView,
    localDisplays: [...connectedView.localDisplays],
  });
  assert.notEqual(a, b);
  assert.equal(sameSharingView(a, b), true);
  assert.equal(sameSharingView(a, a), true);
  // offView carries a null control and empty display arrays where connectedView has neither.
  const off = normalizeSharingView(offView);
  assert.equal(off.control, null);
  assert.equal(sameSharingView(a, off), false);
  // A change buried inside a nested display array is still detected.
  const movedDisplay = normalizeSharingView({
    ...connectedView,
    localDisplays: [{ ...connectedView.localDisplays[0], origin: [10, 0] }],
  });
  assert.equal(sameSharingView(a, movedDisplay), false);
  // A field present on one side only (shape drift) is never mistaken for equal.
  assert.equal(sameSharingView({ x: 1 }, { x: 1, y: null }), false);
  assert.equal(sameSharingView(null, off), false);
  assert.equal(sameSharingView(null, null), true);
});

test("no input-computer or source wording remains in this model", async () => {
  const { readFile } = await import("node:fs/promises");
  const source = await readFile(new URL("sharing-model.mjs", import.meta.url), "utf8");
  assert.doesNotMatch(
    source,
    new RegExp(["source" + "Side", "sharing" + "Role", "choose" + "Source", "input" + "Source"].join("|")),
  );
});
