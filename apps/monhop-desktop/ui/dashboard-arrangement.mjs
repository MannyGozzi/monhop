import {
  VIEW_INSETS,
  arrangementGeometry,
  displayGroups,
  drawnDisplays,
  fitTransform,
  hiddenFromLayout,
  monitorKey,
  placeGroup,
  placementFromLayout,
  sharedMonitors,
  sideRects,
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
const VIEW = { width: 420, height: 190 };
const STALE = "Saved arrangement details need a fresh review.";

export function savedDashboardArrangement(setup) {
  if (!setup?.saved) return unavailable("No saved arrangement is available.");
  const local = displays(setup.localDisplays);
  const peer = displays(setup.peerDisplays);
  const layout = normalizeStoredLayout(setup.previewLayout);
  if (!local || !peer || !layout) return unavailable(STALE);
  const all = [...local, ...peer];
  if (new Set(all.map((display) => display.id)).size !== all.length)
    return unavailable("Saved display identifiers are not valid.");

  const localIds = new Set(local.map((display) => display.id));
  const peerIds = new Set(peer.map((display) => display.id));
  const sourceSide = localIds.has(layout.sourceDisplay)
    ? "local"
    : peerIds.has(layout.sourceDisplay)
      ? "peer"
      : null;
  if (!sourceSide || (setup.sourceSide && setup.sourceSide !== sourceSide))
    return unavailable(STALE);
  const destinationSide = sourceSide === "local" ? "peer" : "local";
  const every = {
    source: sourceSide === "local" ? local : peer,
    destination: destinationSide === "local" ? local : peer,
  };
  // A monitor cabled to both computers is drawn once, on the side the saved layout uses.
  const shared = sharedMonitors(every.source, every.destination);
  const hidden = hiddenFromLayout(layout, every.source, every.destination);
  const source = drawnDisplays(every.source, hidden, shared);
  const destination = drawnDisplays(every.destination, hidden, shared);
  const crossings = crossingsFromLinks(
    layout.links,
    new Set(source.map((display) => display.id)),
    new Set(destination.map((display) => display.id)),
  );
  const groups = displayGroups(source, destination);
  if (!crossings || !groups) return unavailable(STALE);
  // No link was ever recorded: place the two groups the same way a fresh arrangement would, and
  // draw them unconnected rather than inventing a crossing that was never saved.
  const noCrossingYet = crossings.length === 0;
  const placement = noCrossingYet
    ? placeGroup(groups, "right")
    : placementFromLayout(groups, layout.arrangement ?? null, crossings);
  const geometry = placement ? arrangementGeometry(groups, placement) : null;
  if (!geometry || (!noCrossingYet && !geometry.connected))
    return unavailable("Saved arrangement cannot be shown without changing its display geometry.");
  return {
    available: true,
    sourceSide,
    destinationSide,
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
  return arrangement?.placement?.mode === "free"
    ? "Saved display positions, placed one by one. Not a current display check."
    : "Saved display positions. Not a current display check.";
}

export function createDashboardArrangement(setup, names = {}) {
  const arrangement = savedDashboardArrangement(setup);
  const root = document.createElement("figure");
  root.className = "dashboard-arrangement-preview";
  root.dataset.state = arrangement.available ? "saved" : "unavailable";
  if (!arrangement.available) {
    const message = document.createElement("p");
    message.className = "dashboard-arrangement-unavailable";
    message.textContent = arrangement.message;
    root.append(message);
    return root;
  }

  const labels = {
    local: label(names.local, "This computer"),
    peer: label(names.peer, "Saved computer"),
  };
  const platforms = {
    local: platform(names.localPlatform, "macos"),
    peer: platform(names.peerPlatform, "windows"),
  };
  const sides = { source: arrangement.sourceSide, destination: arrangement.destinationSide };
  const groupLabels = {
    source: `${labels[sides.source]} · Input`,
    destination: labels[sides.destination],
  };
  const seamCount = arrangement.seams.length;
  const free = arrangement.placement.mode === "free";

  const svg = svgNode("svg");
  svg.classList.add("dashboard-arrangement-canvas");
  svg.setAttribute("viewBox", `0 0 ${VIEW.width} ${VIEW.height}`);
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  svg.setAttribute("role", "img");
  svg.setAttribute(
    "aria-label",
    `${labels.local} and ${labels.peer}. ${arrangement.noCrossingYet ? "No crossing yet." : `${seamCount} saved display seam${seamCount === 1 ? "" : "s"}.`} ${labels[arrangement.sourceSide]} is the input source.${free ? " Displays were placed one by one." : ""}`,
  );

  const transform = fitTransform(arrangement.tiles, VIEW, VIEW_INSETS);
  const rects = {
    tiles: tileRects(arrangement.tiles, transform),
    sides: sideRects(arrangement.tiles, transform),
  };
  const boxes = labelBoxes(rects.sides, groupLabels, VIEW, VIEW_INSETS);
  const groups = svgNode("g");
  groups.classList.add("arrangement-groups");
  for (const key of ["source", "destination"]) {
    groups.append(
      createGroupNode({
        tiles: arrangement.tiles.filter((tile) => tile.side === key),
        tileRects: rects.tiles,
        groupKey: key,
        platform: platforms[sides[key]],
        side: sides[key],
        label: groupLabels[key],
        rect: rects.sides[key],
        labelBox: boxes[key],
        source: key === "source",
      }),
    );
  }
  svg.append(groups, createSeamLayer(arrangement.seams, transform));

  const legend = createLegend([
    { kind: "group", label: groupLabels.source, side: sides.source },
    { kind: "group", label: groupLabels.destination, side: sides.destination },
    { kind: "primary", label: "Primary display" },
    ...(arrangement.tiles.some((tile) => tile.shared)
      ? [{ kind: "shared", label: "Cabled to both computers" }]
      : []),
    ...(seamCount ? [{ kind: "seam", label: "Pointer crossing" }] : []),
  ]);
  const caption = document.createElement("figcaption");
  caption.textContent = dashboardCaption(arrangement);
  root.append(svg, legend, caption);
  // Real text metrics need a laid-out canvas, so the estimated truncation is corrected on the next frame.
  if (typeof requestAnimationFrame === "function") requestAnimationFrame(() => refineText(svg));
  return root;
}

function crossingsFromLinks(links, sourceIds, destinationIds) {
  if (links.length % 2 !== 0) return null;
  const unused = [...links];
  const crossings = [];
  while (unused.length) {
    const first = unused.pop();
    const reciprocalIndex = unused.findIndex((candidate) => reciprocal(candidate, first));
    if (reciprocalIndex < 0) return null;
    const second = unused.splice(reciprocalIndex, 1)[0];
    const forward = [first, second].find(
      (link) => sourceIds.has(link.fromDisplay) && destinationIds.has(link.toDisplay),
    );
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
