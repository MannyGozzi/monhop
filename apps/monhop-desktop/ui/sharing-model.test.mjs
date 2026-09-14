import assert from "node:assert/strict";
import test from "node:test";

import {
  displayGroups,
  groupedPlacement,
  movePlacement,
  placementOffset,
} from "./arrangement-model.mjs";
import {
  MAX_CROSSINGS,
  applySharingView,
  arrangementForSharing,
  arrangementMode,
  arrangementResetTarget,
  beginPending,
  canApplySetup,
  canChooseSource,
  canEditLayout,
  canLoadArrangement,
  canResetArrangement,
  canSaveArrangement,
  destinationDisplays,
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
  setArrangementMode,
  setArrangements,
  setSource,
  settlePending,
  sourceDisplays,
  sourcePlatform,
  validateLayout,
} from "./sharing-model.mjs";

const localId = "18446744073709551615";
const peerId = "18446744073709551614";

const connectedView = {
  phase: "connected",
  revision: "42",
  sourceSide: "peer",
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
  sourceSide: null,
  localDisplays: [],
  peerDisplays: [],
  message: "Not connected. Input is local.",
};

function connected(patch = {}) {
  return applySharingView(initialSharingState(), { ...connectedView, ...patch });
}

function savedLayout() {
  return {
    sourceDisplay: peerId,
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

const groupsOf = (state) => displayGroups(sourceDisplays(state), destinationDisplays(state));
// Drops the other computer's group at a translation from the input computer's group.
const place = (state, offset) => setArrangement(state, groupedPlacement(groupsOf(state), offset));
const offsetOf = (state) =>
  placementOffset(groupsOf(state), arrangementForSharing(state).placement);
const applied = (state) => ({
  ...connectedView,
  sync: { state: "applied", message: "" },
  synchronizedLayout: layoutForSave(state),
});

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

test("the display notice normalizes to a known kind or drops to null", () => {
  assert.deepEqual(normalizeDisplayNotice({ kind: "waiting" }), { kind: "waiting" });
  assert.deepEqual(normalizeDisplayNotice({ kind: "continued" }), { kind: "continued" });
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

test("the display notice banner names the peer only for the kind that needs it", () => {
  const waiting = displayNoticeCopy("waiting", "Noctua Windows PC");
  assert.equal(waiting.title, "Your displays changed");
  assert.equal(waiting.body, "Arrange them to resume sharing with Noctua Windows PC.");
  assert.equal(waiting.primaryLabel, "Arrange displays");
  assert.equal(waiting.secondaryLabel, "Later");
  const continued = displayNoticeCopy("continued", "Noctua Windows PC");
  assert.equal(continued.title, "Your displays changed");
  assert.match(continued.body, /previous arrangement/);
  assert.equal(continued.primaryLabel, "Arrange displays");
  assert.equal(continued.secondaryLabel, "Keep going");
  assert.equal(displayNoticeCopy("bogus", "Noctua Windows PC"), null);
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
  assert.equal(canChooseSource(state), false);
  state = applySharingView(state, connectedView);
  assert.equal(isConnected(state), true);
  assert.equal(canChooseSource(state), true);
  assert.equal(state.source, "peer");
  assert.equal(sourcePlatform(state), "windows");
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
  state = place(state, [1920, 0]);
  const edited = state.layout;
  state = applySharingView(state, appliedView);
  assert.deepEqual(state.layout, edited);
});

test("choosing the input computer by side flips the display groups and clears the draft", () => {
  let state = initializeArrangement(connected());
  assert.deepEqual(
    sourceDisplays(state).map((display) => display.id),
    [peerId],
  );
  assert.equal(state.layout.crossings.length, 1);
  state = setSource(state, "local");
  assert.deepEqual(
    sourceDisplays(state).map((display) => display.id),
    [localId],
  );
  assert.equal(sourcePlatform(state), "macos");
  assert.equal(state.layout.crossings.length, 0);
  assert.equal(setSource(state, "elsewhere"), state);
});

test("a same-platform pair still names both sides distinctly", () => {
  const state = connected({ localPlatform: "macos", peerPlatform: "macos", sourceSide: "local" });
  assert.equal(sourcePlatform(state), "macos");
  assert.equal(sourcePlatform(setSource(state, "peer")), "macos");
});

test("simple crossings create two full-edge reciprocal native links", () => {
  const state = initializeArrangement(connected());
  const result = validateLayout(state);
  assert.equal(result.ok, true);
  assert.equal(result.layout.sourceDisplay, peerId);
  assert.deepEqual(
    result.layout.links.map((link) => [
      link.fromDisplay,
      link.fromEdge,
      link.toDisplay,
      link.toEdge,
      link.toSpan,
      link.hysteresis,
    ]),
    [
      [peerId, "right", localId, "left", [0, 1], 1],
      [localId, "left", peerId, "right", [49 / 1080, 1031 / 1080], 1],
    ],
  );
  assert.deepEqual(result.layout.links[0].fromSpan, [49 / 1080, 1031 / 1080]);
});

test("layouts reject duplicate directed edges, foreign displays, identical IDs, and too many crossings", () => {
  const state = connected();
  const crossing = {
    id: "one",
    fromDisplay: peerId,
    fromEdge: "right",
    toDisplay: localId,
    toEdge: "left",
  };
  for (const layout of [
    { sourceDisplay: peerId, crossings: [crossing, { ...crossing, id: "two" }] },
    { sourceDisplay: peerId, crossings: [{ ...crossing, fromDisplay: "999" }] },
    { sourceDisplay: peerId, crossings: [{ ...crossing, toDisplay: peerId }] },
    {
      sourceDisplay: peerId,
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

test("a crossing cannot use an edge the computer's own desktop already routes", () => {
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
    sourceDisplay: lg.id,
    crossings: [
      { id: "one", fromDisplay: lg.id, fromEdge: "left", toDisplay: mac.id, toEdge: "right" },
    ],
  };
  const blocked = validateLayout(
    connected({ peerDisplays: [lg, dell], localDisplays: [mac] }),
    layout,
  );
  assert.equal(blocked.ok, false);
  assert.match(blocked.message, /left edge of LG already leads to Dell/);
  const shared = connected({
    peerDisplays: [lg, { ...dell, monitor: mac.monitor }],
    localDisplays: [mac],
  });
  assert.equal(validateLayout(shared, layout).ok, true);
});

test("a crossing may reuse an edge only the destination's own desktop already routes", () => {
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
  let state = initializeArrangement(connected({ localDisplays: [macTop, macBottom] }));
  state = setArrangementMode(state, "free");
  // Drag the bottom display under the Windows display so it touches on the same edge its own desktop
  // uses; the top display moves off to the side, touching nothing.
  const placement = {
    mode: "free",
    positions: {
      [peerId]: [0, 0],
      [macTop.id]: [5000, 5000],
      [macBottom.id]: [0, 1080],
    },
  };
  state = setArrangement(state, placement, { id: macBottom.id });
  assert.equal(arrangementForSharing(state).connected, true);
  const validation = validateLayout(state);
  assert.equal(validation.ok, true);
  assert.equal(validation.message, "");
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

test("an applied sync becomes the current layout on both sides and stays applied until it changes", () => {
  let state = initializeArrangement(connected());
  const native = layoutForSave(state);
  state = applySharingView(state, {
    ...connectedView,
    sync: { state: "applied", message: "Layout applied on both computers." },
    synchronizedLayout: native,
  });
  assert.equal(hasAppliedCurrentLayout(state), true);
  assert.equal(layoutSignature(layoutForSave(state)), layoutSignature(native));
  state = place(state, [1920, 0]);
  assert.equal(hasAppliedCurrentLayout(state), false);
});

test("a peer-applied layout with the other side as source is adopted with that source", () => {
  let state = connected();
  const layout = { ...savedLayout(), sourceDisplay: localId };
  state = applySharingView(state, {
    ...connectedView,
    sourceSide: "local",
    sync: { state: "applied", message: "" },
    synchronizedLayout: layout,
  });
  assert.equal(state.source, "local");
  assert.equal(state.layout.sourceDisplay, localId);
  assert.equal(hasAppliedCurrentLayout(state), true);
});

test("a rejected sync leaves the draft and the chosen side untouched", () => {
  let state = initializeArrangement(connected());
  const draft = state.layout;
  state = applySharingView(state, {
    ...connectedView,
    sync: { state: "rejected", message: "The displays changed. Arrange again." },
  });
  assert.deepEqual(state.layout, draft);
  assert.equal(state.source, "peer");
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
  // A free draft with the displays apart has no crossings, yet it still names a display that is gone.
  let apart = setArrangementMode(initializeArrangement(connected()), "free");
  apart = setArrangement(
    apart,
    movePlacement(
      groupsOf(apart),
      arrangementForSharing(apart).placement,
      { id: localId },
      [500, 0],
    ),
    { id: localId },
  );
  assert.equal(apart.layout.crossings.length, 0);
  assert.equal(apart.layout.placement.mode, "free");
  apart = applySharingView(apart, {
    ...connectedView,
    revision: "43",
    peerDisplays: [{ ...connectedView.peerDisplays[0], id: "77" }],
  });
  assert.equal(apart.layout.placement, null);
  assert.match(apart.message, /displays changed/i);
  assert.equal(arrangementForSharing(initializeArrangement(apart)).connected, true);
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

test("visual arrangement saves contact spans and restores its exact group pose", () => {
  let state = initializeArrangement(connected());
  assert.equal(state.layout.sourceDisplay, peerId);
  assert.equal(arrangementForSharing(state).connected, true);
  state = place(state, [1920, 49]);
  const placement = arrangementForSharing(state).placement;
  assert.equal(canApplySetup(state), true);
  const native = layoutForSave(state);
  assert.deepEqual(native.links[0].toSpan, [0, 1]);
  assert.deepEqual(native.links[0].fromSpan, [49 / 1080, 1031 / 1080]);
  assert.deepEqual(native.arrangement, {
    mode: "grouped",
    positions: [
      { display: peerId, x: 0, y: 0 },
      { display: localId, x: 1920, y: 49 },
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
  state = place(state, [1925, 49]);
  assert.deepEqual(offsetOf(state), [1920, 49]);
  assert.equal(arrangementForSharing(state).connected, true);
});

test("free mode places each display on its own and both computers restore the exact positions", () => {
  let state = initializeArrangement(
    connected({
      localDisplays: [
        connectedView.localDisplays[0],
        {
          ...connectedView.localDisplays[0],
          id: "77",
          name: "Mac Side",
          origin: [1512, 0],
          size: [1000, 982],
          primary: false,
        },
      ],
    }),
  );
  assert.equal(arrangementMode(state), "grouped");
  const before = arrangementForSharing(state).placement;
  state = setArrangementMode(state, "free");
  assert.equal(arrangementMode(state), "free");
  assert.deepEqual(arrangementForSharing(state).placement.positions, before.positions);
  assert.equal(setArrangementMode(state, "free"), state);
  // Pull the Mac's second display under the Windows display: a crossing the OS layout cannot express.
  const groups = groupsOf(state);
  const moved = movePlacement(groups, arrangementForSharing(state).placement, { id: "77" }, [
    920 - 3432,
    1080 - 49,
  ]);
  state = setArrangement(state, moved, { id: "77" });
  const geometry = arrangementForSharing(state);
  assert.equal(geometry.valid, true);
  assert.deepEqual(geometry.seams.map((s) => [s.fromDisplay, s.fromEdge, s.toDisplay]).toSorted(), [
    [peerId, "bottom", "77"],
    [peerId, "right", localId],
  ]);
  const native = layoutForSave(state);
  assert.equal(native.arrangement.mode, "free");
  assert.equal(native.arrangement.positions.length, 3);
  assert.equal(native.links.length, 4);
  let peer = applySharingView(connected({ localDisplays: state.view.localDisplays }), {
    ...connectedView,
    localDisplays: state.view.localDisplays,
    sync: { state: "applied", message: "" },
    synchronizedLayout: native,
  });
  assert.equal(arrangementMode(peer), "free");
  assert.deepEqual(arrangementForSharing(peer).placement, geometry.placement);
  assert.equal(hasAppliedCurrentLayout(peer), true);
  // Moving a display somewhere that changes no crossing still counts as an edit to apply again.
  const drifted = movePlacement(groups, geometry.placement, { id: "77" }, [0, 500]);
  peer = setArrangement(peer, drifted, { id: "77" });
  assert.equal(arrangementForSharing(peer).seams.length, 1);
  assert.notEqual(layoutSignature(layoutForSave(peer)), layoutSignature(native));
  assert.equal(hasAppliedCurrentLayout(peer), false);
  // Back to grouped: each computer's own layout returns and the draft stays connected.
  const grouped = setArrangementMode(state, "grouped");
  assert.equal(arrangementMode(grouped), "grouped");
  assert.ok(placementOffset(groupsOf(grouped), arrangementForSharing(grouped).placement));
  assert.equal(arrangementForSharing(grouped).connected, true);
});

test("named arrangements list only well-formed entries and load only for their input computer", () => {
  let state = place(initializeArrangement(connected()), [1920, 300]);
  const native = layoutForSave(state);
  const listed = normalizeArrangements([
    { name: "  Desk  ", sourceSide: "peer", mode: "grouped", crossings: 1, layout: native },
    { name: "Desk", sourceSide: "peer", mode: "grouped", crossings: 1, layout: native },
    { name: "Couch", sourceSide: "local", mode: "free", crossings: 2, layout: null },
    { name: "", sourceSide: "peer", mode: "grouped", crossings: 1, layout: native },
    { name: "Sideways", sourceSide: "peer", mode: "diagonal", crossings: 1, layout: native },
    { name: "x".repeat(65), sourceSide: "peer", mode: "grouped", crossings: 1, layout: native },
  ]);
  assert.deepEqual(
    listed.map((entry) => [
      entry.name,
      entry.sourceSide,
      entry.mode,
      entry.crossings,
      Boolean(entry.layout),
    ]),
    [
      ["Desk", "peer", "grouped", 1, true],
      ["Couch", "local", "free", 2, false],
    ],
  );
  assert.deepEqual(normalizeArrangements("nope"), []);
  state = setArrangements(place(state, [1920, 49]), listed);
  assert.equal(canLoadArrangement(state, "Desk"), true);
  assert.equal(canLoadArrangement(state, "Couch"), false);
  assert.equal(canLoadArrangement(state, "Nope"), false);
  const loaded = loadArrangement(state, "Desk");
  assert.deepEqual(offsetOf(loaded), [1920, 300]);
  assert.equal(loaded.message, "");
  const wrongSide = loadArrangement(
    setArrangements(setSource(state, "local"), [{ ...listed[0], sourceSide: "peer" }]),
    "Desk",
  );
  assert.match(wrongSide.message, /input computer/i);
  assert.equal(canSaveArrangement(state, "Desk"), true);
  assert.equal(canSaveArrangement(state, "   "), false);
  assert.equal(canSaveArrangement(applySharingView(state, offView), "Desk"), false);
  assert.deepEqual(applySharingView(state, offView).arrangements, []);
});

test("an arrangement is automatic only when the native reply says so exactly", () => {
  const native = layoutForSave(place(initializeArrangement(connected()), [1920, 300]));
  const [remembered, named, defaulted] = normalizeArrangements([
    {
      name: "Desk",
      sourceSide: "peer",
      mode: "grouped",
      crossings: 1,
      layout: native,
      automatic: true,
    },
    {
      name: "Couch",
      sourceSide: "local",
      mode: "free",
      crossings: 2,
      layout: native,
      automatic: false,
    },
    {
      name: "Loft",
      sourceSide: "local",
      mode: "free",
      crossings: 2,
      layout: native,
      automatic: "yes",
    },
  ]);
  assert.equal(remembered.automatic, true);
  assert.equal(named.automatic, false);
  assert.equal(defaulted.automatic, false);
});

test("stored layouts carry their arrangement only when it is well-formed", () => {
  const layout = savedLayout();
  assert.equal(normalizeStoredLayout(layout).arrangement, undefined);
  const positions = [
    { display: peerId, x: 0, y: 0 },
    { display: localId, x: 1920, y: 0.5 },
  ];
  assert.deepEqual(
    normalizeStoredLayout({ ...layout, arrangement: { mode: "free", positions } }).arrangement,
    { mode: "free", positions },
  );
  assert.deepEqual(
    normalizeStoredLayout({ ...layout, arrangement: { mode: "free", positions, hidden: ["7"] } })
      .arrangement,
    { mode: "free", positions, hidden: ["7"] },
  );
  for (const arrangement of [
    { mode: "loose", positions },
    { mode: "free", positions, hidden: [peerId] },
    { mode: "free", positions, hidden: ["7", "7"] },
    { mode: "free", positions, hidden: "none" },
    { mode: "free", positions: [...positions, { display: peerId, x: 1, y: 1 }] },
    { mode: "free", positions: [{ display: "01", x: 0, y: 0 }] },
    { mode: "free", positions: [{ display: peerId, x: Infinity, y: 0 }] },
    { mode: "free", positions: "everywhere" },
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

test("this computer's role and the last drop survive normalization or default off", () => {
  const sharing = normalizeSharingView({
    ...offView,
    phase: "sharing",
    sharingActive: true,
    sharingRole: "sends",
    lastFailure: "The other computer stopped answering. [Session: Wire]",
  });
  assert.equal(sharing.recognized, true);
  assert.equal(sharing.sharingRole, "sends");
  assert.match(sharing.lastFailure, /stopped answering/);
  const defaults = normalizeSharingView(offView);
  assert.equal(defaults.sharingRole, null);
  assert.equal(defaults.lastFailure, "");
  assert.equal(normalizeSharingView({ ...offView, sharingRole: "both" }).sharingRole, null);
  assert.equal(normalizeSharingView({ ...offView, lastFailure: "x".repeat(1001) }).lastFailure, "");
});

test("a running sharing session leaves the screen usable and still accepts a new setup link", () => {
  const live = applySharingView(initialSharingState(), {
    ...offView,
    phase: "sharing",
    sharingActive: true,
    sharingRole: "sends",
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
    sharingRole: "sends",
  });
  assert.equal(hasAppliedLayout(shared), true);
  assert.equal(hasAppliedLayout(place(state, [1920, 300])), false);
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
    sharingRole: "sends",
    sync: appliedSync,
  });
  assert.equal(stopping.syncApplied, true);
  assert.equal(hasAppliedLayout(stopping), true);
  const off = applySharingView(stopping, {
    ...offView,
    phase: "off",
    sharingRole: "sends",
    sync: appliedSync,
  });
  assert.equal(hasAppliedLayout(off), true);
  // The supervisor takes over: still applied, and still not an error.
  assert.equal(
    hasAppliedLayout(
      applySharingView(off, {
        ...offView,
        phase: "starting",
        sharingRole: "sends",
        message: "Connecting to the other computer for sharing.",
      }),
    ),
    true,
  );
});

test("an overlapping or unusable drop is never stored as the layout", () => {
  let state = initializeArrangement(connected());
  const overlapping = place(state, [960, 100]);
  const geometry = arrangementForSharing(overlapping);
  assert.equal(geometry.valid, true);
  assert.equal(geometry.connected, true);
  assert.ok(overlapping.layout.crossings.length > 0);
  assert.equal(validateLayout(overlapping).ok, true);
  assert.equal(place(state, [NaN, 0]), state);
  assert.equal(place(state, [Infinity, 0]), state);
  assert.equal(setArrangement(state, { mode: "free", positions: { [peerId]: [0, 0] } }), state);
});

test("reset goes back to the applied arrangement, and is unavailable while nothing differs", () => {
  let state = initializeArrangement(connected());
  assert.equal(arrangementResetTarget(state).origin, "default");
  assert.equal(canResetArrangement(state), false);
  state = place(state, [1920, 300]);
  assert.equal(canResetArrangement(state), true);
  assert.deepEqual(
    placementOffset(groupsOf(state), arrangementResetTarget(state).placement),
    [1920, 49],
  );
  state = setArrangement(state, arrangementResetTarget(state).placement);
  assert.equal(canResetArrangement(state), false);

  state = place(state, [1920, 300]);
  state = applySharingView(state, applied(state));
  assert.equal(canResetArrangement(state), false);
  state = place(state, [1920, 49]);
  const target = arrangementResetTarget(state);
  assert.equal(target.origin, "applied");
  assert.deepEqual(placementOffset(groupsOf(state), target.placement), [1920, 300]);
  assert.equal(canResetArrangement(state), true);
  assert.deepEqual(offsetOf(setArrangement(state, target.placement)), [1920, 300]);
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
  // The input computer (Windows) keeps its copy by default; the Mac's copy leaves the picture.
  assert.deepEqual(hiddenDisplayIds(state), ["77"]);
  assert.deepEqual(
    sourceDisplays(state).map((d) => [d.id, d.shared === true]),
    [
      [peerId, false],
      ["88", true],
    ],
  );
  assert.deepEqual(
    destinationDisplays(state).map((d) => d.id),
    [localId],
  );
  assert.deepEqual(sharedMonitorChoices(state), [
    { monitor: sharedKey, name: "Desk Monitor", side: "source", canSwap: true },
  ]);
  assert.equal(arrangementForSharing(state).connected, true);
  const native = layoutForSave(state);
  assert.deepEqual(native.arrangement.hidden, ["77"]);
  assert.ok(native.links.every((link) => link.fromDisplay !== "77" && link.toDisplay !== "77"));
  // Mark the Mac as the computer showing on it: the tile changes sides and the picture stays connected.
  const marked = setMonitorSide(state, sharedKey, "destination");
  assert.deepEqual(hiddenDisplayIds(marked), ["88"]);
  assert.deepEqual(
    sourceDisplays(marked).map((d) => d.id),
    [peerId],
  );
  assert.deepEqual(
    destinationDisplays(marked).map((d) => [d.id, d.shared === true]),
    [
      [localId, false],
      ["77", true],
    ],
  );
  assert.deepEqual(sharedMonitorChoices(marked), [
    { monitor: sharedKey, name: "Desk Monitor", side: "destination", canSwap: true },
  ]);
  assert.equal(arrangementForSharing(marked).connected, true);
  assert.equal(setMonitorSide(marked, sharedKey, "destination"), marked);
  assert.equal(setMonitorSide(marked, "0000-0000-00000000", "source"), marked);
  // The other computer adopts the applied layout with the same copy hidden.
  const appliedLayout = layoutForSave(marked);
  assert.deepEqual(appliedLayout.arrangement.hidden, ["88"]);
  let peer = applySharingView(connected(twinView), {
    ...connectedView,
    ...twinView,
    sync: { state: "applied", message: "" },
    synchronizedLayout: appliedLayout,
  });
  assert.deepEqual(hiddenDisplayIds(peer), ["88"]);
  assert.equal(hasAppliedCurrentLayout(peer), true);
  // Reset goes back to the applied picture, including which computer shows on the monitor.
  const flipped = setMonitorSide(peer, sharedKey, "source");
  assert.deepEqual(hiddenDisplayIds(flipped), ["77"]);
  assert.equal(hasAppliedCurrentLayout(flipped), false);
  assert.equal(canResetArrangement(flipped), true);
  const reset = resetArrangement(flipped);
  assert.deepEqual(hiddenDisplayIds(reset), ["88"]);
  assert.equal(hasAppliedCurrentLayout(reset), true);
  assert.equal(canResetArrangement(reset), false);
  // Reset also brings back the applied starting display when the draft had picked another one.
  const otherStart = applySharingView(connected(twinView), {
    ...connectedView,
    ...twinView,
    sync: { state: "applied", message: "" },
    synchronizedLayout: { ...native, sourceDisplay: "88" },
  });
  assert.equal(otherStart.layout.sourceDisplay, "88");
  const roundTrip = setMonitorSide(
    setMonitorSide(otherStart, sharedKey, "destination"),
    sharedKey,
    "source",
  );
  assert.equal(roundTrip.layout.sourceDisplay, peerId);
  const restarted = resetArrangement(roundTrip);
  assert.equal(restarted.layout.sourceDisplay, "88");
  assert.equal(hasAppliedCurrentLayout(restarted), true);
  // Choosing the other input computer keeps the mark, because it describes the cabling; so does the default once placed.
  assert.deepEqual(hiddenDisplayIds(initializeArrangement(setSource(marked, "local"))), ["88"]);
  assert.deepEqual(
    hiddenDisplayIds(
      initializeArrangement(setSource(initializeArrangement(connected(twinView)), "local")),
    ),
    ["77"],
  );
});

test("free placement keeps a shared monitor's tile in place when the other computer is marked as showing on it", () => {
  let state = setArrangementMode(initializeArrangement(connected(twinView)), "free");
  const before = arrangementForSharing(state).placement.positions["88"];
  state = setMonitorSide(state, sharedKey, "destination");
  assert.equal(arrangementMode(state), "free");
  assert.deepEqual(arrangementForSharing(state).placement.positions["77"], before);
  assert.equal(arrangementForSharing(state).placement.positions["88"], undefined);
  assert.equal(arrangementForSharing(state).valid, true);
});

test("a computer with only the shared monitor cannot give it away, and older layouts still hide the unused copy", () => {
  const lone = { ...windowsTwin, primary: true };
  let state = initializeArrangement(
    connected({ localDisplays: twinView.localDisplays, peerDisplays: [lone] }),
  );
  assert.deepEqual(hiddenDisplayIds(state), ["77"]);
  assert.deepEqual(sharedMonitorChoices(state), [
    { monitor: sharedKey, name: "Desk Monitor", side: "source", canSwap: false },
  ]);
  const refused = setMonitorSide(state, sharedKey, "destination");
  assert.deepEqual(hiddenDisplayIds(refused), ["77"]);
  assert.match(refused.message, /only display/);
  // Links alone say which copy an older layout used: the Windows copy on the Mac's left is the one it crossed into.
  const macSends = { ...twinView, sourceSide: "local" };
  const marked = placeArrangement(
    setMonitorSide(initializeArrangement(connected(macSends)), sharedKey, "destination"),
    "left",
  );
  const { arrangement, ...older } = layoutForSave(marked);
  assert.deepEqual(arrangement.hidden, ["77"]);
  assert.ok(older.links.some((link) => link.toDisplay === "88"));
  const adopted = applySharingView(connected(macSends), {
    ...connectedView,
    ...macSends,
    sync: { state: "applied", message: "" },
    synchronizedLayout: older,
  });
  assert.deepEqual(hiddenDisplayIds(adopted), ["77"]);
  assert.equal(arrangementForSharing(adopted).connected, true);
  // Any display can be marked not in use, not only a copy of a shared monitor.
  assert.deepEqual(hiddenDisplayIds(adopted, { hidden: [localId] }), [localId]);
});

test("any display can be marked not in use, and the last one a computer has stays", () => {
  let state = initializeArrangement(connected(twinView));
  assert.deepEqual(hiddenDisplayIds(state), ["77"]);
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
    source: [choice(peerId, "source"), choice("88", "source")],
    destination: [
      choice(localId, "destination", { canLeave: false }),
      choice("77", "destination", { inUse: false }),
    ],
  });
  // Both copies of the shared monitor can be in use at once; the picture stays connected.
  const both = setDisplayInUse(state, "77", true);
  assert.deepEqual(hiddenDisplayIds(both), []);
  assert.equal(arrangementForSharing(both).connected, true);
  assert.deepEqual(
    destinationDisplays(both).map((d) => d.id),
    [localId, "77"],
  );
  assert.deepEqual(sharedMonitorChoices(both), []);
  assert.deepEqual(layoutForSave(both).arrangement.hidden, []);
  // Turning the Windows copy off then reads like the mark on the shared monitor.
  const flipped = setDisplayInUse(both, "88", false);
  assert.deepEqual(hiddenDisplayIds(flipped), ["88"]);
  assert.deepEqual(sharedMonitorChoices(flipped), [
    { monitor: sharedKey, name: "Desk Monitor", side: "destination", canSwap: true },
  ]);
  // A primary display can leave too, but not the last one its computer has.
  const noDesk = setDisplayInUse(state, peerId, false);
  assert.deepEqual(hiddenDisplayIds(noDesk), [peerId, "77"]);
  assert.deepEqual(
    sourceDisplays(noDesk).map((d) => d.id),
    ["88"],
  );
  assert.equal(noDesk.layout.sourceDisplay, "88");
  assert.equal(arrangementForSharing(noDesk).connected, true);
  assert.deepEqual(
    displayUseChoices(noDesk).source.map((d) => [d.id, d.inUse, d.canLeave]),
    [
      [peerId, false, true],
      ["88", true, false],
    ],
  );
  const refused = setDisplayInUse(noDesk, "88", false);
  assert.deepEqual(hiddenDisplayIds(refused), [peerId, "77"]);
  assert.match(refused.message, /only display/);
  // The refusal never trades a marked display for the one being turned off, whatever their order.
  const keepMac = setDisplayInUse(state, localId, false);
  assert.deepEqual(hiddenDisplayIds(keepMac), ["77"]);
  assert.match(keepMac.message, /only display/);
  assert.deepEqual(
    displayUseChoices(state).destination.map((d) => [d.id, d.inUse, d.canLeave]),
    [
      [localId, true, false],
      ["77", false, true],
    ],
  );
  // Nothing changes for a mark that already holds or a display nobody reports.
  assert.equal(setDisplayInUse(state, "77", false), state);
  assert.equal(setDisplayInUse(state, "88", true), state);
  assert.equal(setDisplayInUse(state, "9", false), state);
  assert.equal(
    setDisplayInUse(applySharingView(state, offView), "88", false).layout.hidden,
    state.layout.hidden,
  );
  // Placed one by one, a display coming back lands beside its computer's others at its own offset.
  const free = setArrangementMode(state, "free");
  const back = setDisplayInUse(free, "77", true);
  const positions = arrangementForSharing(back).placement.positions;
  assert.deepEqual(
    [positions["77"][0] - positions[localId][0], positions["77"][1] - positions[localId][1]],
    [1512, 0],
  );
  assert.equal(arrangementForSharing(back).valid, true);
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
  const hiddenAgain = setDisplayInUse(peer, "77", false);
  assert.deepEqual(hiddenDisplayIds(hiddenAgain), ["77"]);
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
  // offView carries a null sourceSide and empty display arrays where connectedView has neither.
  const off = normalizeSharingView(offView);
  assert.equal(off.sourceSide, null);
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
