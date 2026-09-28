import {
  VIEW_INSETS,
  arrangementGeometry,
  chainPlacement,
  computerGroups,
  drawnDisplaysAcross,
  fitTransform,
  groupRects,
  hiddenDisplaysAcross,
  hiddenFromLayout,
  monitorKey,
  placementFromLayout,
  sharedMonitorsAcross,
  tileRects,
} from "./arrangement-model.mjs";
import {
  createGroupNode,
  createLegend,
  createSeamLayer,
  labelBoxes,
  refineText,
  svgNode,
} from "./arrangement-render.mjs";
import { normalizeStoredLayout } from "./sharing-model.mjs";

const MAX_U64 = "18446744073709551615";
const MAX_COORDINATE = 20_000_000;
const VIEW = { width: 640, height: 150 };
const PREVIEW_INSETS = VIEW_INSETS;
const STALE = "Saved arrangement details need a fresh review.";
// Below this a move is a rounding difference, not a rearrangement worth animating.
const MOVE_EPSILON = 0.5;
// Where each viewport last drew each display, so the next drawing can start from there.
const lastDrawn = new Map();

// A forgotten computer's viewports never draw again, so what they last drew is dead weight and
// would only make a reused key slide in from a stranger's positions. The key names the screen and
// the computer, so the computer's fingerprint identifies every viewport that drew it. Returns how
// many were dropped, which is what a caller can assert on.
export function forgetArrangementMotion(motionKeyPrefix) {
  const fragment = String(motionKeyPrefix ?? "");
  if (!fragment) return 0;
  let dropped = 0;
  for (const key of lastDrawn.keys())
    if (key.includes(fragment)) {
      lastDrawn.delete(key);
      dropped += 1;
    }
  return dropped;
}

// The computers a saved setup names: a group record's own `members` array when it has one, or
// today's local/peer saved-setup shape as a two-member fallback. Either way the result is the same
// `{key, displays}` list `computerGroups` and the N-computer helpers take.
function membersOf(setup) {
  if (Array.isArray(setup?.members) && setup.members.length) {
    const normalized = setup.members.map((entry) => ({
      key: typeof entry?.key === "string" && entry.key ? entry.key : null,
      displays: displays(entry?.displays),
    }));
    return normalized.every((member) => member.key && member.displays) ? normalized : null;
  }
  const local = displays(setup?.localDisplays);
  const peer = displays(setup?.peerDisplays);
  return local && peer
    ? [
        { key: "local", displays: local },
        { key: "peer", displays: peer },
      ]
    : null;
}

// The exact two-computer rule (`hiddenFromLayout`) when `members` really is today's local/peer
// shape, so every existing caller keeps its byte-identical behavior, including inferring which copy
// of a shared monitor a legacy save's crossing already uses. A genuine N-member group record has no
// such crossing-side inference (that history does not exist for more than two computers yet): it
// only honors an explicit `hidden` list, else falls back to the same "keep local, else the first
// listed member" default `hiddenDisplaysAcross` uses everywhere else.
function hiddenFromLayoutAcross(layout, members) {
  if (members.length === 2 && members[0].key === "local" && members[1].key === "peer")
    return hiddenFromLayout(layout, members[0].displays, members[1].displays);
  const chosen = layout?.arrangement?.hidden;
  return hiddenDisplaysAcross(members, Array.isArray(chosen) ? chosen : null);
}

export function savedDashboardArrangement(setup) {
  if (!setup?.saved) return unavailable("No saved arrangement is available.");
  const members = membersOf(setup);
  const layout = normalizeStoredLayout(setup.previewLayout);
  if (!members || !layout) return unavailable(STALE);
  const all = members.flatMap((member) => member.displays);
  if (new Set(all.map((display) => display.id)).size !== all.length)
    return unavailable("Saved display identifiers are not valid.");

  // A monitor cabled to more than one of these computers is drawn once, on the side the saved
  // layout uses.
  const shared = sharedMonitorsAcross(members);
  const hidden = hiddenFromLayoutAcross(layout, members);
  const drawnMembers = members.map((member) => ({
    key: member.key,
    displays: drawnDisplaysAcross(member.displays, hidden, shared),
  }));
  const crossings = crossingsFromLinks(
    layout.links,
    new Map(drawnMembers.map((member) => [member.key, new Set(member.displays.map((d) => d.id))])),
  );
  const groups = computerGroups(drawnMembers);
  if (!crossings || !groups) return unavailable(STALE);
  // No link was ever recorded: place the groups the same way a fresh arrangement would, and draw
  // them unconnected rather than inventing a crossing that was never saved.
  const noCrossingYet = crossings.length === 0;
  const placement = noCrossingYet
    ? chainPlacement(groups, "right")
    : placementFromLayout(groups, layout.arrangement ?? null, crossings);
  const geometry = placement ? arrangementGeometry(groups, placement) : null;
  if (!geometry || (!noCrossingYet && !geometry.connected))
    return unavailable("Saved arrangement cannot be shown without changing its display geometry.");
  return {
    available: true,
    noCrossingYet,
    groups: geometry.groups,
    placement: structuredClone(geometry.placement),
    tiles: geometry.tiles.map((tile) => ({ ...tile })),
    seams: noCrossingYet
      ? []
      : geometry.seams.map((seam) => ({ ...seam, start: [...seam.start], end: [...seam.end] })),
  };
}

// A saved layout with no crossing is a real state, not a broken one: the preview says what is
// missing instead of warning that the details need a review.
export function dashboardCaption(arrangement) {
  if (arrangement?.noCrossingYet) return "No crossing yet. Arrange the displays to connect them.";
  return DRAWN_DISPLAYS_NOTE;
}

// The diagram follows this computer's displays live and the other computer's as last reported.
export const DRAWN_DISPLAYS_NOTE = "Displays as each computer last reported them.";

// What changed between two drawings of one viewport: how far each display moved, as the inverse
// translate a FLIP transition starts from, and which displays are on screen for the first time.
export function arrangementMotion(previous, current) {
  const moved = {};
  const entered = [];
  for (const [id, rect] of Object.entries(current ?? {})) {
    const was = previous?.[id];
    if (!was) {
      entered.push(id);
      continue;
    }
    const delta = [was.x - rect.x, was.y - rect.y];
    if (delta.some((value) => Math.abs(value) >= MOVE_EPSILON)) moved[id] = delta;
  }
  return { moved, entered };
}

function motionEnabled() {
  return !matchMedia("(prefers-reduced-motion: reduce)").matches && !document.hidden;
}

// The drawing is rebuilt from nothing every render, so a display that moved is placed where it
// belongs and then offset back to where it was; clearing the offset a frame later slides it over.
// A display that vanished left with its node, so only arrivals are faded.
function playMotion(svg, motion) {
  const started = [];
  for (const [id, [dx, dy]] of Object.entries(motion.moved)) {
    const node = tileNode(svg, id);
    if (!node) continue;
    node.style.transform = `translate(${dx}px, ${dy}px)`;
    started.push(node);
  }
  for (const id of motion.entered) {
    const node = tileNode(svg, id);
    if (node) node.dataset.motion = "enter";
  }
  if (!started.length) return;
  requestAnimationFrame(() => {
    // Reading the box lays the offset out, so clearing it below is a change the transition can run.
    for (const node of started) node.getBoundingClientRect();
    for (const node of started) {
      node.dataset.motion = "settle";
      node.style.transform = "";
    }
  });
}

// Display identifiers are digits only, so they go straight into a selector.
function tileNode(svg, id) {
  return svg.querySelector(`.arrangement-monitor[data-display="${id}"]`);
}

export function createDashboardArrangement(setup, names = {}) {
  const arrangement = savedDashboardArrangement(setup);
  const root = document.createElement("figure");
  root.className = "dashboard-arrangement-preview";
  if (typeof names.transitionName === "string")
    root.dataset.sharedTransition = names.transitionName;
  root.dataset.state = arrangement.available ? "saved" : "unavailable";
  if (names.compact) root.dataset.size = "compact";
  if (names.compactLegend) root.dataset.size = "home";
  if (!arrangement.available) {
    // Nothing is drawn, so the remembered positions would only make the next drawing slide in
    // from where a different arrangement once sat.
    if (names.motionKey) lastDrawn.delete(names.motionKey);
    const message = document.createElement("p");
    message.className = "dashboard-arrangement-unavailable";
    message.textContent = arrangement.message;
    root.append(message);
    return root;
  }

  const members = memberPresentation(names, arrangement.groups.order);
  const labels = Object.fromEntries(members.map((member) => [member.key, member.label]));
  const seamCount = arrangement.seams.length;

  const svg = svgNode("svg");
  svg.classList.add("dashboard-arrangement-canvas");
  svg.setAttribute("viewBox", `0 0 ${VIEW.width} ${VIEW.height}`);
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  svg.setAttribute("role", "img");
  svg.setAttribute(
    "aria-label",
    `${joinLabels(members.map((member) => member.label))}. ${arrangement.noCrossingYet ? "No crossing yet." : `${seamCount} saved display seam${seamCount === 1 ? "" : "s"}.`}`,
  );

  const transform = fitTransform(arrangement.tiles, VIEW, PREVIEW_INSETS);
  const rects = {
    tiles: tileRects(arrangement.tiles, transform),
    groups: groupRects(arrangement.tiles, transform),
  };
  const boxes = labelBoxes(rects.groups, labels, VIEW, PREVIEW_INSETS);
  const groupsLayer = svgNode("g");
  groupsLayer.classList.add("arrangement-groups");
  for (const member of members) {
    if (!rects.groups[member.key]) continue;
    groupsLayer.append(
      createGroupNode({
        tiles: arrangement.tiles.filter((tile) => tile.group === member.key),
        tileRects: rects.tiles,
        groupKey: member.key,
        platform: member.platform,
        side: member.key,
        tone: member.tone,
        label: member.label,
        rect: rects.groups[member.key],
        labelBox: boxes[member.key],
      }),
    );
  }
  const seams = createSeamLayer(arrangement.seams, transform);
  svg.append(groupsLayer, seams);

  const seamSignature = JSON.stringify(arrangement.seams.map((seam) => [seam.start, seam.end]));
  if (names.motionKey) {
    const previous = lastDrawn.get(names.motionKey);
    lastDrawn.set(names.motionKey, { tiles: rects.tiles, seams: seamSignature });
    const motion = arrangementMotion(previous?.tiles, rects.tiles);
    if (motionEnabled()) {
      playMotion(svg, motion);
      if (previous?.seams !== seamSignature) seams.dataset.motion = "enter";
    }
  }

  // The compact viewport rides inside a computer card, where the legend would cost more room
  // than it explains; the picture and one caption are what that card needs.
  const legendItems = [
    ...members.map((member) => ({
      kind: "group",
      label: member.label,
      side: member.key,
      tone: member.tone,
    })),
    ...(names.compactLegend ? [] : [{ kind: "primary", label: "Primary display" }]),
    ...(arrangement.tiles.some((tile) => tile.shared)
      ? [{ kind: "shared", label: "Cabled to both computers" }]
      : []),
    ...(seamCount ? [{ kind: "seam", label: "Pointer crossing" }] : []),
  ];
  const legend = names.compact ? null : createLegend(legendItems);
  const captionText =
    names.hideCaption && !arrangement.noCrossingYet
      ? ""
      : names.caption && !arrangement.noCrossingYet
        ? names.caption
        : dashboardCaption(arrangement);
  const caption = captionText ? document.createElement("figcaption") : null;
  if (caption) caption.textContent = captionText;
  root.append(svg, ...(legend ? [legend] : []), ...(caption ? [caption] : []));
  // Real text metrics need a laid-out canvas, so the estimated truncation is corrected on the next frame.
  if (typeof requestAnimationFrame === "function") requestAnimationFrame(() => refineText(svg));
  return root;
}

// The label, platform and color tone for each group in `order`, from a group record's own
// `names.members` list when the caller has one, or from today's `names.local` / `names.peer` /
// `names.localPlatform` / `names.peerPlatform` shape as the two-computer fallback every existing
// caller still passes.
function memberPresentation(names, order) {
  if (Array.isArray(names.members) && names.members.length) {
    const byKey = new Map(names.members.map((entry) => [entry.key, entry]));
    return order.map((key, index) => {
      const entry = byKey.get(key) ?? {};
      return {
        key,
        label: label(entry.label, `Computer ${index + 1}`),
        platform: platform(entry.platform, index === 0 ? "macos" : "windows"),
        tone: typeof entry.tone === "string" && entry.tone ? entry.tone : defaultTone(index),
      };
    });
  }
  return order.map((key, index) => ({
    key,
    label: label(
      key === "local" ? names.local : names.peer,
      key === "local" ? "This computer" : "Saved computer",
    ),
    platform: platform(
      key === "local" ? names.localPlatform : names.peerPlatform,
      index === 0 ? "macos" : "windows",
    ),
    tone: defaultTone(index),
  }));
}

// The color slot a group draws in: the first computer is "local", the second is "peer" (today's two
// tones), and a third through eighth are "peer-2".."peer-6" (the group record caps a group at 8
// members; a ninth would reuse "peer-6" rather than draw in no color at all).
function defaultTone(index) {
  if (index === 0) return "local";
  if (index === 1) return "peer";
  return `peer-${Math.min(index, 6)}`;
}

function joinLabels(labelList) {
  if (labelList.length <= 2) return labelList.join(" and ");
  return `${labelList.slice(0, -1).join(", ")} and ${labelList.at(-1)}`;
}

// Recovers this computer's saved crossings from the raw link pairs the native side stores: every
// crossing is two reciprocal links, and the "forward" half is the one that goes from an earlier
// member (by `idsByMember`'s own key order) to a later one, matching how `arrangementGeometry` only
// ever seams a pair in that direction.
function crossingsFromLinks(links, idsByMember) {
  if (links.length % 2 !== 0) return null;
  const order = [...idsByMember.keys()];
  const memberOf = (id) => order.find((key) => idsByMember.get(key).has(id));
  const unused = [...links];
  const crossings = [];
  while (unused.length) {
    const first = unused.pop();
    const reciprocalIndex = unused.findIndex((candidate) => reciprocal(candidate, first));
    if (reciprocalIndex < 0) return null;
    const second = unused.splice(reciprocalIndex, 1)[0];
    const forward = [first, second].find((link) => {
      const fromMember = memberOf(link.fromDisplay);
      const toMember = memberOf(link.toDisplay);
      return fromMember && toMember && order.indexOf(fromMember) < order.indexOf(toMember);
    });
    if (!forward) return null;
    crossings.push({
      fromDisplay: forward.fromDisplay,
      fromEdge: forward.fromEdge,
      fromSpan: [...forward.fromSpan],
      toDisplay: forward.toDisplay,
      toEdge: forward.toEdge,
      toSpan: [...forward.toSpan],
    });
  }
  return crossings;
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

function displays(value) {
  if (!Array.isArray(value) || value.length === 0 || value.length > 16) return null;
  const normalized = value.map((display) => {
    const id = displayId(display?.id);
    const origin = point(display?.origin, false);
    const size = point(display?.size, true);
    if (!id || !origin || !size) return null;
    return {
      id,
      name: typeof display.name === "string" && display.name ? display.name : "Display",
      origin,
      size,
      primary: display.primary === true,
      monitor: monitorKey(display.monitor),
    };
  });
  return normalized.every(Boolean) ? normalized : null;
}

function unavailable(message) {
  return { available: false, message };
}

function displayId(value) {
  return typeof value === "string" &&
    (value === "0" || /^[1-9][0-9]{0,19}$/.test(value)) &&
    (value.length < MAX_U64.length || (value.length === MAX_U64.length && value <= MAX_U64))
    ? value
    : null;
}

function point(value, positive) {
  return Array.isArray(value) &&
    value.length === 2 &&
    value.every(Number.isFinite) &&
    value.every((number) => Math.abs(number) <= MAX_COORDINATE) &&
    (!positive || value.every((number) => number > 1))
    ? [...value]
    : null;
}

function sameSpan(left, right) {
  return left[0] === right[0] && left[1] === right[1];
}

function label(value, fallback) {
  return typeof value === "string" && value.trim() ? value.trim().slice(0, 80) : fallback;
}

function platform(value, fallback) {
  return value === "windows" || value === "macos" ? value : fallback;
}
