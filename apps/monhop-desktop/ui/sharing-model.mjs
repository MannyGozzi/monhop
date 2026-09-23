import {
  arrangementGeometry,
  displayGroups,
  drawnDisplays,
  groupedPlacement,
  hiddenDisplays,
  hiddenFromLayout,
  isPlacement,
  layoutArrangement,
  matchesCrossings,
  monitorKey,
  ownSeams,
  placeGroup,
  placementFromLayout,
  placementOffset,
  resolvePlacement,
  samePlacement,
  seamCrossings,
  sharedMonitors,
  snapPlacement,
} from "./arrangement-model.mjs";

export const MAX_CROSSINGS = 32;
const MAX_LINKS = 64;
const MAX_ARRANGEMENTS = 32;
export const MAX_ARRANGEMENT_NAME = 64;

const PLATFORMS = new Set(["macos", "windows"]);
const SIDES = new Set(["local", "peer"]);
const DISPLAY_NOTICE_KINDS = new Set(["continued", "waiting", "updating", "peerDeciding"]);
const PHASES = new Set([
  "off",
  "connecting",
  "connected",
  "reconnecting",
  "stopping",
  "starting",
  "sharing",
  "error",
]);
// The setup link carries topology and layout proposals; a sharing session runs on its own and outlives it.
const LINK_PHASES = new Set(["connecting", "connected", "reconnecting"]);
const SESSION_PHASES = new Set([...LINK_PHASES, "stopping", "starting", "sharing"]);
const SYNC_STATES = new Set(["idle", "sending", "receiving", "applied", "rejected"]);
const EDGES = new Set(["left", "right", "top", "bottom"]);
const WHOLE_EDGE = [0, 1];
const DEFAULT_HYSTERESIS = 1;
const FINGERPRINT = /^[0-9a-f]{64}$/i;
const MAX_U64 = "18446744073709551615";
const MAX_COORDINATE = 20_000_000;

export function initialSharingState() {
  return {
    generation: 0,
    pending: null,
    view: null,
    layout: emptyLayout(),
    appliedRevision: null,
    appliedSignature: null,
    arrangements: [],
    syncApplied: false,
    message: "",
  };
}

function emptyLayout() {
  // `hidden` names the displays marked not in use; null means the default choice.
  return { placement: null, crossings: [], hidden: null };
}

// One in-flight native command at a time; the generation lets late replies be ignored.
export function beginPending(state, kind) {
  const generation = state.generation + 1;
  return { ...state, generation, pending: { kind, generation }, message: "" };
}

export function isCurrentPending(state, generation) {
  return state.pending?.generation === generation;
}

export function settlePending(state, generation) {
  return isCurrentPending(state, generation) ? { ...state, pending: null } : state;
}

export function failPending(state, generation, message) {
  if (!isCurrentPending(state, generation)) return state;
  return {
    ...state,
    pending: null,
    message: boundedText(message, 1000) || "The action did not finish.",
  };
}

export function applySharingView(state, value) {
  const view = normalizeSharingView(value);
  // An unrecognized reply may hide a live worker, so Stop stays available until a real view arrives.
  if (!view.recognized)
    return {
      ...state,
      view: { ...(state.view ?? view), phase: "unknown", busy: false, message: view.message },
      message: view.message,
    };
  const previous = state.view;
  // The view narrates its own phase on every poll; state.message stays for what only the UI can report.
  let next = { ...state, view, message: "" };
  // Apply closes the link with the sync still "applied", so only a live link with no applied sync clears it.
  next.syncApplied =
    view.sync.state === "applied" || (!LINK_PHASES.has(view.phase) && state.syncApplied === true);
  // The link is gone: keep the draft and the applied receipt; saved arrangements are listed again on reconnect.
  if (!LINK_PHASES.has(view.phase)) return { ...next, arrangements: [] };
  const signature = view.synchronizedLayout ? layoutSignature(view.synchronizedLayout) : null;
  const newlyApplied =
    view.sync.state === "applied" &&
    signature &&
    (state.appliedRevision !== view.revision || state.appliedSignature !== signature);
  // Only a newly applied layout replaces the draft; polling the same applied layout keeps local edits.
  if (newlyApplied) {
    const proposal = draftFromStoredLayout(view.synchronizedLayout, next);
    if (proposal && validateLayout(next, proposal).ok) {
      next = {
        ...next,
        layout: proposal,
        appliedRevision: view.revision,
        appliedSignature: signature,
      };
    }
  }
  // A draft is checked against the displays only once the link is connected; while it dials, nothing is known yet.
  const previouslyConnected = Boolean(previous) && isConnected({ view: previous });
  const revisionChanged = Boolean(previous) && previous.revision !== view.revision;
  if (isConnected(next) && (!previouslyConnected || revisionChanged)) {
    if (!draftFits(next)) {
      next = {
        ...next,
        layout: { ...emptyLayout(), hidden: next.layout.hidden },
        message: revisionChanged ? "The displays changed. Arrange them again." : next.message,
      };
    }
  }
  return next;
}

// A draft is kept only while every display it places still exists and its crossings still validate.
function draftFits(state) {
  const layout = state.layout;
  if (!layout.placement && !layout.crossings.length) return true;
  if (layout.placement && !isPlacement(currentGroups(state), layout.placement)) return false;
  return !layout.crossings.length || validateLayout(state).ok;
}

// Two questions, two answers: the setup link is live, or anything at all is running.
export function isLinkActive(state) {
  return LINK_PHASES.has(state.view?.phase);
}

export function isSessionActive(state) {
  return SESSION_PHASES.has(state.view?.phase);
}

export function isEditingLayout(state) {
  return state.view?.editing === true;
}

// The supervisor keeps the connection to the computer in use without the UI, so polling follows that
// computer rather than anything this screen started. The computer list can name it before the
// sharing view has been read again, so both answers count.
export function shouldPollSharing(state, activeComputer = null) {
  return (
    Boolean(state.view?.active) ||
    Boolean(activeComputer) ||
    isSessionActive(state) ||
    state.view?.phase === "unknown"
  );
}

// The backend marks its own dialing and connecting busy for exclusivity; the user is only held
// back by a command in flight or a stop the backend has not finished.
export function isActionBusy(state) {
  return state.pending !== null || (state.view?.busy === true && state.view.phase === "stopping");
}

export function isConnected(state) {
  return state.view?.phase === "connected" && !state.view.busy;
}

export function isSyncing(state) {
  return ["sending", "receiving"].includes(state.view?.sync?.state);
}

// The supervisor dials and retries by itself, so only work this screen started freezes it.
export function isBusySharing(state) {
  return isActionBusy(state) || isSyncing(state);
}

// Opening the editor adopts a live link with that computer or stops a session and dials one, so only
// a command in flight or an unfinished stop blocks it.
export function canEditLayout(state, interfaceId) {
  return (
    Boolean(boundedText(interfaceId, 512)) &&
    state.pending === null &&
    state.view?.phase !== "stopping"
  );
}

// Every display each computer reports, before shared monitors are folded into one.
function allDisplays(state) {
  if (!isConnected(state)) return { local: [], peer: [] };
  const { localDisplays, peerDisplays } = state.view;
  return { local: localDisplays, peer: peerDisplays };
}

// The displays the arrangement works with: the ones in use, each shared monitor once by default.
function arrangedDisplays(state, layout = state.layout) {
  const all = allDisplays(state);
  const pairs = sharedMonitors(all.local, all.peer);
  const hidden = hiddenDisplays(all.local, all.peer, layout?.hidden ?? null);
  return {
    local: drawnDisplays(all.local, hidden, pairs),
    peer: drawnDisplays(all.peer, hidden, pairs),
    hidden,
    pairs,
    all,
  };
}

export function hiddenDisplayIds(state, layout = state.layout) {
  return arrangedDisplays(state, layout).hidden;
}

// Each monitor cabled to both computers, with the side that draws it and whether the other side could instead.
export function sharedMonitorChoices(state) {
  const { hidden, pairs, all } = arrangedDisplays(state);
  const gone = new Set(hidden);
  return pairs
    .filter((pair) => gone.has(pair.local.id) !== gone.has(pair.peer.id))
    .map((pair) => {
      const side = gone.has(pair.local.id) ? "peer" : "local";
      const other = oppositeSide(side);
      const swapped = hiddenDisplays(all.local, all.peer, [
        ...hidden.filter((id) => id !== pair[other].id),
        pair[side].id,
      ]);
      return {
        monitor: pair.monitor,
        name: pair[side].name,
        side,
        canSwap: swapped.includes(pair[side].id),
      };
    });
}

// Marks which computer shows on a shared monitor: its tile keeps its place and the other copy leaves the picture.
export function setMonitorSide(state, monitor, side) {
  if (!isConnected(state) || isBusySharing(state) || !SIDES.has(side)) return state;
  const { hidden: before, pairs, all } = arrangedDisplays(state);
  const pair = pairs.find((candidate) => candidate.monitor === monitor);
  if (!pair || !before.includes(pair[side].id)) return state;
  const shown = pair[side].id;
  const leaving = pair[oppositeSide(side)].id;
  const hidden = hiddenDisplays(all.local, all.peer, [
    ...before.filter((id) => id !== shown),
    leaving,
  ]);
  if (!hidden.includes(leaving))
    return {
      ...state,
      message: `${pair[side].name} is the only display ${ownerName(side)} has left, so it cannot be shown on this one.`,
    };
  return withHidden(state, hidden);
}

// Every display each computer reports, whether it is in the picture, and whether it could leave it.
export function displayUseChoices(state) {
  const { hidden, pairs, all } = arrangedDisplays(state);
  const gone = new Set(hidden);
  const cabled = new Set(pairs.flatMap((pair) => [pair.local.id, pair.peer.id]));
  const choices = (side) => {
    const inUse = all[side].filter((display) => !gone.has(display.id)).length;
    return all[side].map((display) => ({
      id: display.id,
      side,
      name: display.name,
      size: display.size,
      primary: display.primary === true,
      cabledToBoth: cabled.has(display.id),
      inUse: !gone.has(display.id),
      canLeave: gone.has(display.id) || inUse > 1,
    }));
  };
  return { local: choices("local"), peer: choices("peer") };
}

// Marks a display in use or not, whatever monitor it is on. The last display a computer has stays.
export function setDisplayInUse(state, id, inUse) {
  if (!isConnected(state) || isBusySharing(state)) return state;
  const { hidden: before, all } = arrangedDisplays(state);
  const side = [...SIDES].find((s) => all[s].some((d) => d.id === id));
  if (!side || before.includes(id) !== inUse) return state;
  // Checked before the list is rebuilt, so the displays already marked keep their mark.
  if (!inUse && all[side].filter((d) => !before.includes(d.id)).length <= 1) {
    const name = all[side].find((d) => d.id === id).name;
    return {
      ...state,
      message: `${name} is the only display ${ownerName(side)} has left, so it stays in the picture.`,
    };
  }
  const hidden = hiddenDisplays(
    all.local,
    all.peer,
    inUse ? before.filter((other) => other !== id) : [...before, id],
  );
  return withHidden(state, hidden);
}

function ownerName(side) {
  return side === "local" ? "this computer" : "the other computer";
}

// Redraws the picture with `hidden` changed. Each computer's own block keeps its internal layout,
// so hiding or showing a display only ever needs the same translation between the two blocks.
function withHidden(state, hidden) {
  const next = { ...state, layout: { ...state.layout, hidden } };
  const groups = currentGroups(next);
  const current = arrangementForSharing(state).placement;
  const moving = { side: "peer" };
  const offset = current ? placementOffset(currentGroups(state), current) : null;
  const placement =
    (offset && resolvePlacement(groups, groupedPlacement(groups, offset), moving)) ||
    placeGroup(groups, "right");
  const placed = placement ? setArrangement(next, placement, moving) : next;
  return placed !== next
    ? placed
    : initializeArrangement({
        ...next,
        layout: { ...emptyLayout(), hidden },
        syncApplied: false,
      });
}

function oppositeSide(side) {
  return side === "local" ? "peer" : "local";
}

function sameIds(left, right) {
  return left.length === right.length && left.every((id) => right.includes(id));
}

function currentGroups(state, layout = state.layout) {
  const { local, peer } = arrangedDisplays(state, layout);
  return displayGroups(local, peer);
}

export function arrangementForSharing(state) {
  const groups = currentGroups(state);
  const placement =
    state.layout?.placement ??
    placementFromLayout(groups, null, state.layout?.crossings) ??
    placeGroup(groups);
  const geometry = arrangementGeometry(groups, placement);
  if (
    !state.layout?.placement &&
    state.layout?.crossings?.length &&
    !placementFromLayout(groups, null, state.layout.crossings)
  ) {
    return {
      ...geometry,
      valid: false,
      connected: false,
      seams: [],
      message:
        "The saved edge mapping cannot be shown as one arrangement. Place the groups again to replace it.",
    };
  }
  return geometry;
}

export function initializeArrangement(state) {
  if (!isConnected(state) || state.layout?.placement || state.layout?.crossings?.length)
    return state;
  return placeArrangement(state, "right");
}

// `moving` names the computer block that was dragged; the other one stays fixed.
export function setArrangement(state, placement, moving = { side: "peer" }, snapDistance = 0) {
  if (!isConnected(state) || isBusySharing(state)) return state;
  const groups = currentGroups(state);
  if (!isPlacement(groups, placement)) return state;
  // Only a placement the pointer can really use is stored; anything else leaves the arrangement untouched.
  const target = resolvePlacement(
    groups,
    snapPlacement(groups, placement, moving, snapDistance),
    moving,
  );
  if (!target) return state;
  const geometry = arrangementGeometry(groups, target);
  const layout = {
    placement: target,
    crossings: geometry.connected ? seamCrossings(geometry.seams) : [],
    hidden: state.layout.hidden ?? hiddenDisplayIds(state),
  };
  // A move that resolves back to the same picture changes nothing.
  if (sameDraft(state.layout, layout)) return state;
  return { ...state, layout, syncApplied: false };
}

function sameDraft(current, next) {
  if (!sameIds(current.hidden ?? [], next.hidden ?? [])) return false;
  if (current.placement && !samePlacement(current.placement, next.placement)) return false;
  const currentCrossings = current.crossings ?? [];
  // A draft loaded from crossings alone is pinned by them; identical crossings mean the same picture.
  if (!current.placement && !currentCrossings.length) return false;
  return (
    currentCrossings.length === next.crossings.length &&
    next.crossings.every((crossing) =>
      currentCrossings.some((other) => sameCrossing(crossing, other)),
    )
  );
}

function sameCrossing(left, right) {
  return (
    ["fromDisplay", "fromEdge", "toDisplay", "toEdge"].every((key) => left[key] === right[key]) &&
    ["fromSpan", "toSpan"].every((key) =>
      (left[key] ?? WHOLE_EDGE).every(
        (value, index) => Math.abs(value - (right[key] ?? WHOLE_EDGE)[index]) <= 1e-7,
      ),
    )
  );
}

// Quick placement moves the peer computer's whole block.
export function placeArrangement(state, side) {
  const placement = placeGroup(currentGroups(state), side);
  return placement ? setArrangement(state, placement) : state;
}

// Reset restores what both computers already run, and falls back to the default placement before anything is applied.
export function arrangementResetTarget(state) {
  const groups = currentGroups(state);
  if (!groups) return null;
  const applied = appliedDraft(state);
  if (applied) return { placement: applied.placement, hidden: applied.hidden, origin: "applied" };
  const placement = placeGroup(groups, "right");
  return placement ? { placement, hidden: hiddenDisplayIds(state), origin: "default" } : null;
}

export function canResetArrangement(state) {
  if (!isConnected(state) || isBusySharing(state)) return false;
  const target = arrangementResetTarget(state);
  if (!target) return false;
  const current = arrangementForSharing(state).placement;
  return !(samePlacement(target.placement, current) && sameIds(target.hidden, hiddenDisplayIds(state)));
}

// Reset takes the whole applied picture back: placement and hidden copies.
export function resetArrangement(state) {
  const target = arrangementResetTarget(state);
  if (!target || !canResetArrangement(state)) return state;
  const base = { ...state, layout: { ...emptyLayout(), hidden: target.hidden } };
  const placed = setArrangement(base, target.placement, { side: "peer" });
  return placed === base ? state : placed;
}

function appliedDraft(state) {
  const proposal = state.view?.synchronizedLayout
    ? draftFromStoredLayout(state.view.synchronizedLayout, state)
    : null;
  return proposal?.crossings?.length && proposal.placement ? proposal : null;
}

export function layoutForSave(state) {
  const validation = validateLayout(state, state.layout);
  return validation.ok ? validation.layout : null;
}

export function validateLayout(state, layout = state.layout) {
  if (!isConnected(state))
    return invalidLayout("Wait until both computers are connected, then arrange the displays.");
  if (
    !layout ||
    typeof layout !== "object" ||
    !Array.isArray(layout.crossings) ||
    layout.crossings.length === 0 ||
    layout.crossings.length > MAX_CROSSINGS
  )
    return invalidLayout("Move the displays together until they touch.");
  const { local, peer, hidden, all } = arrangedDisplays(state, layout);
  const localIds = new Set(local.map((display) => display.id));
  const peerIds = new Set(peer.map((display) => display.id));

  const links = [];
  const fromEdges = [];
  for (const crossing of layout.crossings) {
    if (
      !isCrossing(crossing) ||
      !localIds.has(crossing.fromDisplay) ||
      !peerIds.has(crossing.toDisplay) ||
      crossing.fromDisplay === crossing.toDisplay
    )
      return invalidLayout("Choose current displays on opposite computers for every crossing.");
    const forward = {
      ...wholeEdgeLink(
        crossing.fromDisplay,
        crossing.fromEdge,
        crossing.toDisplay,
        crossing.toEdge,
      ),
      fromSpan: [...(crossing.fromSpan ?? WHOLE_EDGE)],
      toSpan: [...(crossing.toSpan ?? WHOLE_EDGE)],
    };
    const reverse = {
      ...wholeEdgeLink(
        crossing.toDisplay,
        crossing.toEdge,
        crossing.fromDisplay,
        crossing.fromEdge,
      ),
      fromSpan: [...forward.toSpan],
      toSpan: [...forward.fromSpan],
    };
    for (const link of [forward, reverse]) {
      if (fromEdges.some((other) => edgeSpansOverlap(link, other)))
        return invalidLayout("Crossings on the same display edge must not overlap.");
      fromEdges.push(link);
      links.push(link);
    }
  }
  if (links.length > MAX_LINKS)
    return invalidLayout("The layout supports at most 64 directed edges.");
  // Either computer's cursor can cross now, so a crossing may not share an edge span either
  // computer's own desktop already routes to one of its own neighbours.
  const ownSeamsEitherSide = [...ownSeams(all.local, hidden), ...ownSeams(all.peer, hidden)];
  for (const link of links) {
    const seam = ownSeamsEitherSide.find((other) => edgeSpansOverlap(link, other));
    if (seam) return invalidLayout(ownSeamMessage(seam, [...all.local, ...all.peer]));
  }
  const groups = displayGroups(local, peer);
  if (
    layout.placement &&
    (!isPlacement(groups, layout.placement) ||
      !matchesCrossings(groups, layout.placement, layout.crossings))
  ) {
    return invalidLayout("Place the displays so their highlighted edges meet.");
  }
  const native = { links };
  if (layout.placement) native.arrangement = layoutArrangement(layout.placement, hidden);
  return { ok: true, layout: native, message: "" };
}

export function canApplySetup(state) {
  if (!isConnected(state) || isBusySharing(state)) return false;
  return arrangementForSharing(state).connected && validateLayout(state).ok;
}

// The link closes as soon as both computers save the layout, so the applied receipt has to outlive it.
export function hasAppliedLayout(state) {
  return state.syncApplied === true && (hasAppliedCurrentLayout(state) || !isConnected(state));
}

export function hasAppliedCurrentLayout(state) {
  const view = state.view;
  if (
    !isConnected(state) ||
    view.sync.state !== "applied" ||
    state.appliedRevision !== view.revision
  )
    return false;
  const current = layoutForSave(state);
  return Boolean(current) && layoutSignature(current) === state.appliedSignature;
}

export function layoutSignature(layout) {
  if (!Array.isArray(layout?.links)) return "";
  const links = layout.links
    .map((link) =>
      JSON.stringify([
        link.fromDisplay,
        link.fromEdge,
        link.fromSpan,
        link.toDisplay,
        link.toEdge,
        link.toSpan,
        link.hysteresis,
      ]),
    )
    .toSorted();
  const arrangement = layout.arrangement
    ? [layout.arrangement.positions.map((p) => [p.display, p.x, p.y]), layout.arrangement.hidden ?? []]
    : null;
  return JSON.stringify([links, arrangement]);
}

// --- named arrangements --------------------------------------------------

export function normalizeArrangements(value) {
  if (!Array.isArray(value) || value.length > MAX_ARRANGEMENTS) return [];
  const seen = new Set();
  const entries = [];
  for (const item of value) {
    const source = item && typeof item === "object" ? item : {};
    const name = normalizeArrangementName(source.name);
    if (!name || seen.has(name)) continue;
    seen.add(name);
    const crossings =
      Number.isSafeInteger(source.crossings) && source.crossings >= 0 ? source.crossings : 0;
    entries.push({
      name,
      crossings,
      layout: normalizeStoredLayout(source.layout),
      automatic: source.automatic === true,
      fits: source.fits === true,
    });
  }
  return entries;
}

export function normalizeArrangementName(value) {
  if (typeof value !== "string") return "";
  const trimmed = value.trim();
  return trimmed && Array.from(trimmed).length <= MAX_ARRANGEMENT_NAME && !/[\p{Cc}]/u.test(trimmed)
    ? trimmed
    : "";
}

export function setArrangements(state, value) {
  return { ...state, arrangements: normalizeArrangements(value) };
}

export function arrangementByName(state, name) {
  const wanted = normalizeArrangementName(name);
  return wanted ? (state.arrangements.find((entry) => entry.name === wanted) ?? null) : null;
}

// A saved arrangement can be loaded only while it fits both computers' current displays.
export function canLoadArrangement(state, name) {
  const entry = arrangementByName(state, name);
  return Boolean(entry?.layout) && isConnected(state) && !isBusySharing(state);
}

export function canSaveArrangement(state, name) {
  return (
    Boolean(normalizeArrangementName(name)) &&
    isConnected(state) &&
    !isBusySharing(state) &&
    Boolean(layoutForSave(state))
  );
}

export function loadArrangement(state, name) {
  if (!canLoadArrangement(state, name))
    return {
      ...state,
      message: "This arrangement no longer fits the connected displays. Arrange them again.",
    };
  return loadArrangementLayout(state, arrangementByName(state, name).layout);
}

// The one place a stored layout becomes the draft. The connected editor arrives here by name and
// a computer card by the entry it listed, so a layout that no longer fits says so either way.
export function loadArrangementLayout(state, layout) {
  const proposal = layout ? draftFromStoredLayout(layout, state) : null;
  if (!proposal || !proposal.placement || !validateLayout(state, proposal).ok)
    return {
      ...state,
      message: "This arrangement no longer fits the connected displays. Arrange them again.",
    };
  return { ...state, layout: proposal, syncApplied: false, message: "" };
}

// --- native views --------------------------------------------------------

// Structural equality between two normalized sharing views, so a poll that changes nothing can skip a render.
export function sameSharingView(a, b) {
  if (a === b) return true;
  if (a === null || b === null || typeof a !== "object" || typeof b !== "object") return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a))
    return a.length === b.length && a.every((item, index) => sameSharingView(item, b[index]));
  const keysA = Object.keys(a);
  const keysB = Object.keys(b);
  return (
    keysA.length === keysB.length &&
    keysA.every((key) => Object.hasOwn(b, key) && sameSharingView(a[key], b[key]))
  );
}

export function normalizeDisplayNotice(value) {
  return value && typeof value === "object" && DISPLAY_NOTICE_KINDS.has(value.kind)
    ? { kind: value.kind }
    : null;
}

// Only a display change the user has to settle interrupts them with a banner. A change MonHop is
// already handling, here or on the other computer, is a line on the computer's own card.
const DISPLAY_NOTICE_COPY = {
  waiting: {
    presentation: "banner",
    title: "Your displays changed",
    body: () => "No saved layout fits. Arrange the displays to start sharing.",
    primaryLabel: "Arrange displays",
    secondaryLabel: "Dismiss",
  },
  // Raised whenever the rebuilt layout had to drop a crossing, a position or a display, which
  // includes a monitor that was simply unplugged: nothing was "left out" there, so the copy says
  // what is true of every case and offers the arrange screen for the ones where it is not enough.
  continued: {
    presentation: "banner",
    title: "Your displays changed",
    body: () =>
      "Sharing continues with a layout adapted to them. Arrange the displays if you want something different.",
    primaryLabel: "Arrange displays",
    secondaryLabel: "Keep going",
  },
  updating: {
    presentation: "inline",
    title: null,
    body: () => "Updating the layout…",
    primaryLabel: null,
    secondaryLabel: null,
  },
  peerDeciding: {
    presentation: "inline",
    title: null,
    body: (peerName) => `${peerName} is choosing the layout.`,
    primaryLabel: null,
    secondaryLabel: null,
  },
};

// Where a notice belongs: "banner" is the card that stops the page, "inline" is the status line
// on the computer's own card. An unknown kind is drawn nowhere.
export function noticePresentation(kind) {
  return DISPLAY_NOTICE_COPY[kind]?.presentation ?? null;
}

// The notice's copy is a pure function of its kind, so no screen re-derives it from the view.
export function displayNoticeCopy(kind, peerName) {
  const copy = DISPLAY_NOTICE_COPY[kind];
  return copy
    ? {
        presentation: copy.presentation,
        title: copy.title,
        body: copy.body(peerName),
        primaryLabel: copy.primaryLabel,
        secondaryLabel: copy.secondaryLabel,
      }
    : null;
}

export function normalizeSharingView(value) {
  const source = value && typeof value === "object" ? value : {};
  const phase = PHASES.has(source.phase) ? source.phase : null;
  const localDisplays = normalizeDisplays(source.localDisplays);
  const peerDisplays = normalizeDisplays(source.peerDisplays);
  const displayIds = new Set(
    [...(localDisplays ?? []), ...(peerDisplays ?? [])].map((display) => display.id),
  );
  const connected = phase === "connected";
  const validConnected =
    !connected ||
    Boolean(
      localDisplays?.length &&
      peerDisplays?.length &&
      displayIds.size === localDisplays.length + peerDisplays.length,
    );
  const link = source.link && typeof source.link === "object" ? source.link : {};
  const sync = source.sync && typeof source.sync === "object" ? source.sync : {};
  if (
    !phase ||
    !validConnected ||
    !revision(source.revision) ||
    !localDisplays ||
    !peerDisplays ||
    typeof source.busy !== "boolean" ||
    typeof source.sharingActive !== "boolean"
  ) {
    return {
      recognized: false,
      phase: "error",
      revision: null,
      localPlatform: null,
      peerPlatform: null,
      localDisplays: [],
      peerDisplays: [],
      busy: false,
      sharingActive: false,
      peerFingerprint: null,
      active: null,
      editing: false,
      control: null,
      lastFailure: "",
      link: { attempt: 0, since: "" },
      sync: { state: "idle", message: "" },
      synchronizedLayout: null,
      displayNotice: null,
      message: "The connection state was not recognized. Stop, then connect again.",
    };
  }
  return {
    recognized: true,
    phase,
    revision: revision(source.revision),
    localPlatform: PLATFORMS.has(source.localPlatform) ? source.localPlatform : null,
    peerPlatform: PLATFORMS.has(source.peerPlatform) ? source.peerPlatform : null,
    localDisplays: localDisplays ?? [],
    peerDisplays: peerDisplays ?? [],
    busy: source.busy === true,
    sharingActive: source.sharingActive === true,
    peerFingerprint: fingerprint(source.peerFingerprint),
    active: fingerprint(source.active),
    editing: source.editing === true,
    control: normalizeControl(source.control),
    lastFailure: boundedText(source.lastFailure, 1000),
    message: boundedText(source.message, 1000),
    link: {
      attempt: Number.isSafeInteger(link.attempt) && link.attempt >= 0 ? link.attempt : 0,
      since: boundedText(link.since, 64),
    },
    sync: {
      state: SYNC_STATES.has(sync.state) ? sync.state : "idle",
      message: boundedText(sync.message, 1000),
    },
    synchronizedLayout: normalizeStoredLayout(source.synchronizedLayout),
    displayNotice: normalizeDisplayNotice(source.displayNotice),
  };
}

// At least one direction must stay on; a reply claiming neither is damaged, not a real state.
function normalizeControl(value) {
  if (
    !value ||
    typeof value !== "object" ||
    typeof value.localToPeer !== "boolean" ||
    typeof value.peerToLocal !== "boolean" ||
    (!value.localToPeer && !value.peerToLocal)
  )
    return null;
  return {
    localToPeer: value.localToPeer,
    peerToLocal: value.peerToLocal,
    syncing: value.syncing === true,
  };
}

function normalizeDisplays(value) {
  if (!Array.isArray(value) || value.length > 16) return null;
  const displays = value.map(normalizeDisplay);
  if (displays.some((item) => !item)) return null;
  const ids = new Set();
  for (const display of displays) {
    if (ids.has(display.id)) return null;
    ids.add(display.id);
  }
  return displays;
}

function normalizeDisplay(value) {
  const source = value && typeof value === "object" ? value : {};
  const id = nullableText(source.id, 20);
  const name = nullableText(source.name, 512);
  const origin = point(source.origin, false);
  const size = point(source.size, true);
  if (
    !id ||
    !isDisplayId(id) ||
    !name ||
    !origin ||
    !size ||
    !finitePositive(source.scale) ||
    typeof source.primary !== "boolean"
  )
    return null;
  const monitor = monitorKey(source.monitor);
  if (source.monitor !== undefined && source.monitor !== null && !monitor) return null;
  return { id, name, origin, size, scale: source.scale, primary: source.primary, monitor };
}

export function normalizeStoredLayout(value) {
  if (
    !value ||
    typeof value !== "object" ||
    !Array.isArray(value.links) ||
    value.links.length > MAX_LINKS
  )
    return null;
  const links = value.links.map(normalizeLink);
  const fromEdges = [];
  for (const link of links) {
    if (!link) return null;
    if (fromEdges.some((other) => edgeSpansOverlap(link, other))) return null;
    fromEdges.push(link);
  }
  const layout = { links };
  if (value.arrangement !== undefined && value.arrangement !== null) {
    const arrangement = normalizeArrangement(value.arrangement);
    if (!arrangement) return null;
    layout.arrangement = arrangement;
  }
  return layout;
}

function normalizeArrangement(value) {
  if (
    !value ||
    typeof value !== "object" ||
    !Array.isArray(value.positions) ||
    value.positions.length > MAX_ARRANGEMENTS
  )
    return null;
  const seen = new Set();
  const positions = [];
  for (const entry of value.positions) {
    const display = entry && typeof entry === "object" ? entry.display : null;
    if (!isDisplayId(display) || seen.has(display) || !point([entry.x, entry.y], false))
      return null;
    seen.add(display);
    positions.push({ display, x: entry.x, y: entry.y });
  }
  // A layout saved before displays could be marked not in use carries no list, and stays that way.
  if (value.hidden === undefined) return { positions };
  if (!Array.isArray(value.hidden) || value.hidden.length > MAX_ARRANGEMENTS) return null;
  const hidden = [];
  for (const display of value.hidden) {
    if (!isDisplayId(display) || seen.has(display)) return null;
    seen.add(display);
    hidden.push(display);
  }
  return { positions, hidden };
}

function normalizeLink(value) {
  const source = value && typeof value === "object" ? value : {};
  const ids = [source.fromDisplay, source.toDisplay];
  if (
    !ids.every(isDisplayId) ||
    ids[0] === ids[1] ||
    !EDGES.has(source.fromEdge) ||
    !EDGES.has(source.toEdge) ||
    !span(source.fromSpan) ||
    !span(source.toSpan) ||
    !finitePositive(source.hysteresis)
  )
    return null;
  return {
    fromDisplay: source.fromDisplay,
    fromEdge: source.fromEdge,
    fromSpan: [...source.fromSpan],
    toDisplay: source.toDisplay,
    toEdge: source.toEdge,
    toSpan: [...source.toSpan],
    hysteresis: source.hysteresis,
  };
}

// A stored layout back into a draft: its crossings, plus the placement when the positions still fit.
function draftFromStoredLayout(layout, state) {
  if (!layout || !Array.isArray(layout.links) || layout.links.length % 2 !== 0) return null;
  const all = allDisplays(state);
  const unused = [...layout.links];
  const crossings = [];
  while (unused.length) {
    const first = unused.pop();
    const reverseIndex = unused.findIndex((candidate) => reciprocal(candidate, first));
    if (reverseIndex < 0) return null;
    const second = unused.splice(reverseIndex, 1)[0];
    const forward = [first, second].find(
      (link) =>
        all.local.some((display) => display.id === link.fromDisplay) &&
        all.peer.some((display) => display.id === link.toDisplay),
    );
    if (!forward || forward.hysteresis !== DEFAULT_HYSTERESIS) return null;
    crossings.push({
      id: `saved-${crossings.length + 1}`,
      fromDisplay: forward.fromDisplay,
      fromEdge: forward.fromEdge,
      toDisplay: forward.toDisplay,
      toEdge: forward.toEdge,
      fromSpan: [...forward.fromSpan],
      toSpan: [...forward.toSpan],
    });
  }
  const hidden = hiddenFromLayout(layout, all.local, all.peer);
  const placement = placementFromLayout(
    currentGroups(state, { hidden }),
    layout.arrangement ?? null,
    crossings,
  );
  return { placement, crossings, hidden };
}

function reciprocal(left, right) {
  return (
    left.fromDisplay === right.toDisplay &&
    left.fromEdge === right.toEdge &&
    left.toDisplay === right.fromDisplay &&
    left.toEdge === right.fromEdge &&
    sameSpan(left.fromSpan, right.toSpan) &&
    sameSpan(left.toSpan, right.fromSpan) &&
    left.hysteresis === right.hysteresis
  );
}

function isCrossing(value) {
  return (
    value &&
    typeof value === "object" &&
    typeof value.id === "string" &&
    isDisplayId(value.fromDisplay) &&
    isDisplayId(value.toDisplay) &&
    EDGES.has(value.fromEdge) &&
    EDGES.has(value.toEdge) &&
    span(value.fromSpan ?? WHOLE_EDGE) &&
    span(value.toSpan ?? WHOLE_EDGE)
  );
}

function wholeEdgeLink(fromDisplay, fromEdge, toDisplay, toEdge) {
  return {
    fromDisplay,
    fromEdge,
    fromSpan: [...WHOLE_EDGE],
    toDisplay,
    toEdge,
    toSpan: [...WHOLE_EDGE],
    hysteresis: DEFAULT_HYSTERESIS,
  };
}

// A crossing cannot sit on an edge span either computer's own desktop already routes to a neighbour.
function ownSeamMessage(seam, displays) {
  const name = (id) => displays.find((d) => d.id === id)?.name ?? "a display";
  return `The ${seam.fromEdge} edge of ${name(seam.fromDisplay)} already leads to ${name(seam.toDisplay)} on the same computer. Use a free edge, or mark which computer shows on a monitor cabled to both.`;
}

function invalidLayout(message) {
  return { ok: false, layout: null, message };
}

function point(value, positive) {
  if (
    !Array.isArray(value) ||
    value.length !== 2 ||
    !value.every(Number.isFinite) ||
    value.some((part) => Math.abs(part) > MAX_COORDINATE)
  )
    return null;
  if (positive && !value.every((part) => part > 0)) return null;
  return [...value];
}

function span(value) {
  return (
    Array.isArray(value) &&
    value.length === 2 &&
    Number.isFinite(value[0]) &&
    Number.isFinite(value[1]) &&
    value[0] >= 0 &&
    value[1] <= 1 &&
    value[0] < value[1]
  );
}

function sameSpan(left, right) {
  return (
    Array.isArray(left) &&
    Array.isArray(right) &&
    left.length === 2 &&
    right.length === 2 &&
    left[0] === right[0] &&
    left[1] === right[1]
  );
}

function isDisplayId(value) {
  return isCanonicalU64(value);
}

function isCanonicalU64(value) {
  return (
    typeof value === "string" &&
    (value === "0" || /^[1-9][0-9]{0,19}$/.test(value)) &&
    (value.length < MAX_U64.length || (value.length === MAX_U64.length && value <= MAX_U64))
  );
}

function finitePositive(value) {
  return Number.isFinite(value) && value > 0;
}

function revision(value) {
  return isCanonicalU64(value) ? value : null;
}

function fingerprint(value) {
  return typeof value === "string" && FINGERPRINT.test(value) ? value.toLowerCase() : null;
}

function boundedText(value, maximum) {
  return typeof value === "string" && value.length <= maximum ? value : "";
}

function nullableText(value, maximum) {
  const normalized = boundedText(value, maximum);
  return normalized || null;
}

function edgeSpansOverlap(left, right) {
  return (
    left.fromDisplay === right.fromDisplay &&
    left.fromEdge === right.fromEdge &&
    left.fromSpan[0] < right.fromSpan[1] &&
    right.fromSpan[0] < left.fromSpan[1]
  );
}
