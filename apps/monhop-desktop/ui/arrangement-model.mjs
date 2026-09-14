// Geometry is in each OS's reported desktop units, not physical screen dimensions.
// A placement puts every display on one shared canvas: grouped keeps each computer's own layout and
// moves it as a block; free places each display on its own. Links between the computers come from
// the edges that touch, whichever mode produced them.
const EPSILON = 1e-7;
const MAX_COORDINATE = 20_000_000;
const MIN_CONTACT = 1;
const LABEL_GAP = 5;
const OPPOSITE = { left: "right", right: "left", top: "bottom", bottom: "top" };
const SIDES = ["source", "destination"];
// vendor-product-serial of the physical monitor, as the native side formats it.
const MONITOR_KEY = /^[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{8}$/;

export const MODES = Object.freeze(["grouped", "free"]);

// One label/copy per mode: `shortLabel` for the displays list, the rest for the arrangement editor.
export const MODE_INFO = Object.freeze({
  grouped: {
    shortLabel: "Own layouts",
    label: "Each computer's own layout",
    hint: "Displays stay where each computer's system settings put them; drag a computer as one block.",
    instructions:
      "Drag a computer against the other; the edge where they touch is where the pointer crosses. With a computer selected, arrow keys nudge it and Shift with an arrow moves it further.",
  },
  free: {
    shortLabel: "Placed one by one",
    label: "Place each display",
    hint: "Drag any display on its own. Edges that touch a display of the other computer become crossings.",
    instructions:
      "Drag any display; every edge that touches a display of the other computer is a crossing. With a display selected, arrow keys nudge it and Shift with an arrow moves it further.",
  },
});

// Top and bottom insets reserve the band a group label needs, so a label never lands off the canvas.
export const VIEW_INSETS = Object.freeze({ top: 24, right: 16, bottom: 24, left: 16 });

export function displayGroups(source, destination) {
  const groups = { source: group(source), destination: group(destination) };
  return groups.source && groups.destination ? groups : null;
}

function group(displays) {
  if (!Array.isArray(displays) || !displays.length || displays.length > 16) return null;
  if (
    displays.some(
      (d) =>
        !d ||
        typeof d.id !== "string" ||
        !point(d.origin) ||
        !point(d.size) ||
        d.size.some((n) => n <= 1),
    )
  )
    return null;
  const minX = Math.min(...displays.map((d) => d.origin[0]));
  const minY = Math.min(...displays.map((d) => d.origin[1]));
  const rectangles = displays.map((d) => ({
    id: d.id,
    name: d.name,
    primary: d.primary,
    shared: d.shared === true,
    monitor: typeof d.monitor === "string" ? d.monitor : null,
    x: d.origin[0] - minX,
    y: d.origin[1] - minY,
    width: d.size[0],
    height: d.size[1],
  }));
  return {
    width: Math.max(...rectangles.map(right)),
    height: Math.max(...rectangles.map(bottom)),
    displays: rectangles,
  };
}

// --- shared monitors -----------------------------------------------------

export function monitorKey(value) {
  return typeof value === "string" && MONITOR_KEY.test(value) ? value : null;
}

// One physical monitor cabled to both computers appears on each side; only a key unique on its side identifies it.
export function sharedMonitors(source, destination) {
  const sources = uniqueMonitors(source);
  const destinations = uniqueMonitors(destination);
  return [...sources]
    .filter(([key]) => destinations.has(key))
    .map(([monitor, a]) => ({ monitor, source: a, destination: destinations.get(monitor) }));
}

function uniqueMonitors(displays) {
  const counts = new Map();
  for (const d of Array.isArray(displays) ? displays : [])
    if (typeof d?.monitor === "string" && d.monitor)
      counts.set(d.monitor, (counts.get(d.monitor) ?? 0) + 1);
  const unique = new Map();
  for (const d of Array.isArray(displays) ? displays : [])
    if (counts.get(d?.monitor) === 1) unique.set(d.monitor, d);
  return unique;
}

// The displays left out of the picture: the ones `chosen` marks not in use, or by default the other
// computer's copy of each monitor cabled to both. A computer always keeps at least one display.
export function hiddenDisplays(source, destination, chosen = null) {
  if (Array.isArray(chosen)) {
    const wanted = new Set(chosen);
    const keep = keeper(source, destination);
    for (const side of ["source", "destination"])
      for (const display of side === "source" ? source : destination)
        if (wanted.has(display.id)) keep.hide(side, display.id);
    return keep.hidden;
  }
  return oneCopyEach(source, destination, () => "destination");
}

// One copy of every shared monitor leaves, trying `first(pair)`'s side before the other.
function oneCopyEach(source, destination, first) {
  const keep = keeper(source, destination);
  for (const pair of sharedMonitors(source, destination)) {
    const side = first(pair);
    const other = side === "source" ? "destination" : "source";
    if (!keep.hide(side, pair[side].id)) keep.hide(other, pair[other].id);
  }
  return keep.hidden;
}

function keeper(source, destination) {
  const remaining = { source: source.length, destination: destination.length };
  const hidden = [];
  return {
    hidden,
    hide(side, id) {
      if (remaining[side] <= 1 || hidden.includes(id)) return false;
      remaining[side] -= 1;
      hidden.push(id);
      return true;
    },
  };
}

// A stored layout lists the displays it left out. One saved before displays could be marked not in
// use only shows which copy of a shared monitor it used: the one its starting display or links touch.
export function hiddenFromLayout(layout, source, destination) {
  const chosen = layout?.arrangement?.hidden;
  if (Array.isArray(chosen)) return hiddenDisplays(source, destination, chosen);
  const used = new Set([
    layout?.sourceDisplay,
    ...(layout?.links ?? []).flatMap((link) => [link.fromDisplay, link.toDisplay]),
  ]);
  return oneCopyEach(source, destination, (pair) =>
    used.has(pair.destination.id) && !used.has(pair.source.id) ? "source" : "destination",
  );
}

// What one side draws: its hidden copies gone, the displays standing for a shared monitor marked.
export function drawnDisplays(displays, hidden, shared) {
  const gone = new Set(hidden);
  const marked = new Set(shared.flatMap((pair) => [pair.source.id, pair.destination.id]));
  return displays
    .filter((d) => !gone.has(d.id))
    .map((d) => (marked.has(d.id) ? { ...d, shared: true } : d));
}

// The seams a computer's own desktop already has between its visible displays, derived the way the
// native side does: a crossing to the other computer cannot share such an edge span.
export function ownSeams(displays, hidden = []) {
  const gone = new Set(hidden);
  const visible = (Array.isArray(displays) ? displays : []).filter(
    (d) => d && !gone.has(d.id) && point(d.origin) && point(d.size),
  );
  const seams = [];
  for (const [index, a] of visible.entries())
    for (const b of visible.slice(index + 1)) {
      const x = [
        Math.max(a.origin[0], b.origin[0]),
        Math.min(a.origin[0] + a.size[0], b.origin[0] + b.size[0]),
      ];
      const y = [
        Math.max(a.origin[1], b.origin[1]),
        Math.min(a.origin[1] + a.size[1], b.origin[1] + b.size[1]),
      ];
      if (y[0] < y[1]) {
        if (a.origin[0] + a.size[0] === b.origin[0])
          seams.push(...ownSeamPair(a, b, "right", "left", y));
        else if (b.origin[0] + b.size[0] === a.origin[0])
          seams.push(...ownSeamPair(a, b, "left", "right", y));
      }
      if (x[0] < x[1]) {
        if (a.origin[1] + a.size[1] === b.origin[1])
          seams.push(...ownSeamPair(a, b, "bottom", "top", x));
        else if (b.origin[1] + b.size[1] === a.origin[1])
          seams.push(...ownSeamPair(a, b, "top", "bottom", x));
      }
    }
  return seams;
}

function ownSeamPair(a, b, fromEdge, toEdge, overlap) {
  const vertical = fromEdge === "left" || fromEdge === "right";
  const span = (d) => {
    const origin = vertical ? d.origin[1] : d.origin[0];
    const size = vertical ? d.size[1] : d.size[0];
    return [(overlap[0] - origin) / size, (overlap[1] - origin) / size];
  };
  const fromSpan = span(a);
  const toSpan = span(b);
  return [
    { fromDisplay: a.id, fromEdge, fromSpan, toDisplay: b.id, toEdge, toSpan },
    {
      fromDisplay: b.id,
      fromEdge: toEdge,
      fromSpan: toSpan,
      toDisplay: a.id,
      toEdge: fromEdge,
      toSpan: fromSpan,
    },
  ];
}

// --- placements ----------------------------------------------------------

export function groupedPlacement(groups, offset) {
  if (!groups || !point(offset)) return null;
  const positions = {};
  for (const d of groups.source.displays) positions[d.id] = [d.x, d.y];
  for (const d of groups.destination.displays) positions[d.id] = [d.x + offset[0], d.y + offset[1]];
  return { mode: "grouped", positions };
}

// The destination translation, when every display still sits where its own computer puts it.
export function placementOffset(groups, placement) {
  if (!groups || !isPlacement(groups, placement)) return null;
  const anchor = groups.source.displays[0];
  const base = placement.positions[anchor.id];
  const shift = [base[0] - anchor.x, base[1] - anchor.y];
  const reference = groups.destination.displays[0];
  const offset = [
    placement.positions[reference.id][0] - reference.x - shift[0],
    placement.positions[reference.id][1] - reference.y - shift[1],
  ];
  const expected = groupedPlacement(groups, offset);
  for (const side of SIDES)
    for (const d of groups[side].displays) {
      const [x, y] = placement.positions[d.id];
      if (
        !near(x - shift[0], expected.positions[d.id][0]) ||
        !near(y - shift[1], expected.positions[d.id][1])
      )
        return null;
    }
  return offset;
}

// Exactly one finite position per connected display, and none for anything else.
export function isPlacement(groups, placement) {
  if (
    !groups ||
    !placement ||
    !MODES.includes(placement.mode) ||
    !placement.positions ||
    typeof placement.positions !== "object"
  )
    return false;
  const ids = SIDES.flatMap((side) => groups[side].displays.map((d) => d.id));
  return (
    ids.every((id) => point(placement.positions[id])) &&
    Object.keys(placement.positions).length === ids.length
  );
}

export function samePlacement(a, b) {
  if (!a || !b || a.mode !== b.mode) return false;
  const ids = Object.keys(a.positions);
  return (
    ids.length === Object.keys(b.positions).length &&
    ids.every(
      (id) => b.positions[id] && a.positions[id].every((v, i) => near(v, b.positions[id][i])),
    )
  );
}

export function tiles(groups, placement) {
  return SIDES.flatMap((side) =>
    groups[side].displays.map((d) => ({
      ...d,
      side,
      x: placement.positions[d.id][0],
      y: placement.positions[d.id][1],
    })),
  );
}

function withPositions(placement, update) {
  const positions = { ...placement.positions };
  for (const [id, value] of Object.entries(update)) positions[id] = [...value];
  return { mode: placement.mode, positions };
}

// A move is one display (free) or one whole computer (grouped); `moving` names which.
export function movingIds(groups, placement, moving) {
  if (!groups || !moving) return [];
  if (moving.id) return placement.positions[moving.id] ? [moving.id] : [];
  if (SIDES.includes(moving.side)) return groups[moving.side].displays.map((d) => d.id);
  return [];
}

export function movePlacement(groups, placement, moving, delta) {
  if (!isPlacement(groups, placement) || !point(delta)) return placement;
  const update = {};
  for (const id of movingIds(groups, placement, moving))
    update[id] = [placement.positions[id][0] + delta[0], placement.positions[id][1] + delta[1]];
  return withPositions(placement, update);
}

// Free mode starts from the grouped picture, so nothing jumps when the toggle flips.
export function toFree(placement) {
  return placement ? { mode: "free", positions: structuredClone(placement.positions) } : null;
}

// Back to grouped: each computer's own layout returns, placed where its displays sat on average.
export function toGrouped(groups, placement) {
  if (!isPlacement(groups, placement)) return null;
  if (placement.mode === "grouped") return placement;
  const centre = (side) => {
    const own = groups[side].displays;
    const dx = own.reduce((sum, d) => sum + placement.positions[d.id][0] - d.x, 0) / own.length;
    const dy = own.reduce((sum, d) => sum + placement.positions[d.id][1] - d.y, 0) / own.length;
    return [dx, dy];
  };
  const source = centre("source");
  const destination = centre("destination");
  const offset = [destination[0] - source[0], destination[1] - source[1]];
  return (
    resolvePlacement(groups, groupedPlacement(groups, offset), { side: "destination" }) ??
    placeGroup(groups, "right")
  );
}

// --- geometry ------------------------------------------------------------

export function arrangementGeometry(groups, placement) {
  const empty = { groups, placement, tiles: [], seams: [], connected: false };
  if (!groups || !isPlacement(groups, placement))
    return { ...empty, valid: false, message: "Check the displays before arranging them." };
  const all = tiles(groups, placement);
  for (const g of Object.values(groups)) {
    if (g.displays.some((a, i) => g.displays.slice(i + 1).some((b) => overlaps(a, b)))) {
      return {
        ...empty,
        tiles: all,
        valid: false,
        message: "Mirrored or overlapping displays need to be arranged in system settings first.",
      };
    }
  }
  const free = placement.mode === "free";
  if (all.some((a, i) => all.slice(i + 1).some((b) => overlaps(a, b)))) {
    return {
      ...empty,
      tiles: all,
      valid: false,
      message: free
        ? "Move the displays apart so none of them overlap."
        : "Move the computer groups beside each other, without overlap.",
    };
  }
  const seams = all
    .filter((t) => t.side === "source")
    .flatMap((a) => all.filter((t) => t.side === "destination").flatMap((b) => contacts(a, b)));
  if (seams.length > 32)
    return {
      ...empty,
      tiles: all,
      valid: false,
      message: "This arrangement has too many separate crossings.",
    };
  const idle = free
    ? "Drag a display until it touches one from the other computer."
    : "Drag a computer group until its displays touch the other group.";
  return {
    ...empty,
    tiles: all,
    valid: true,
    seams,
    connected: seams.length > 0,
    message: seams.length ? "Highlighted edges let the pointer cross in both directions." : idle,
  };
}

function contacts(a, b) {
  const found = [];
  const y0 = Math.max(a.y, b.y),
    y1 = Math.min(bottom(a), bottom(b));
  const x0 = Math.max(a.x, b.x),
    x1 = Math.min(right(a), right(b));
  if (y1 - y0 > EPSILON) {
    if (near(right(a), b.x)) found.push(seam(a, b, "right", "left", y0, y1, right(a), true));
    if (near(a.x, right(b))) found.push(seam(a, b, "left", "right", y0, y1, a.x, true));
  }
  if (x1 - x0 > EPSILON) {
    if (near(bottom(a), b.y)) found.push(seam(a, b, "bottom", "top", x0, x1, bottom(a), false));
    if (near(a.y, bottom(b))) found.push(seam(a, b, "top", "bottom", x0, x1, a.y, false));
  }
  return found;
}
function seam(a, b, fromEdge, toEdge, begin, end, fixed, vertical) {
  const span = (r) =>
    [begin, end].map((p) =>
      Math.max(0, Math.min(1, (p - (vertical ? r.y : r.x)) / (vertical ? r.height : r.width))),
    );
  return {
    fromDisplay: a.id,
    toDisplay: b.id,
    fromEdge,
    toEdge,
    fromSpan: span(a),
    toSpan: span(b),
    start: vertical ? [fixed, begin] : [begin, fixed],
    end: vertical ? [fixed, end] : [end, fixed],
  };
}
function sameSeam(a, b) {
  return (
    ["fromDisplay", "toDisplay", "fromEdge", "toEdge"].every((k) => a[k] === b[k]) &&
    ["fromSpan", "toSpan"].every((k) => (a[k] ?? [0, 1]).every((v, i) => near(v, b[k][i])))
  );
}

export function seamCrossings(seams) {
  return seams.map((s, i) => ({
    id: `seam-${i + 1}`,
    fromDisplay: s.fromDisplay,
    fromEdge: s.fromEdge,
    fromSpan: [...s.fromSpan],
    toDisplay: s.toDisplay,
    toEdge: s.toEdge,
    toSpan: [...s.toSpan],
  }));
}

// --- snapping and drop resolution ----------------------------------------

// Nudges the moving displays onto a nearby edge of anything they do not carry along.
export function snapPlacement(groups, placement, moving, distance = 0) {
  if (!isPlacement(groups, placement) || !Number.isFinite(distance) || distance <= 0)
    return placement;
  const ids = new Set(movingIds(groups, placement, moving));
  if (!ids.size) return placement;
  const all = tiles(groups, placement);
  const carried = all.filter((t) => ids.has(t.id));
  const fixed = all.filter((t) => !ids.has(t.id));
  // Each axis snaps on its own, so a display can line up sideways and vertically in one drop.
  const best = { x: null, y: null };
  for (const a of fixed)
    for (const b of carried) {
      for (const [axis, shift] of [
        ["x", right(a) - b.x],
        ["x", a.x - right(b)],
        ["y", bottom(a) - b.y],
        ["y", a.y - bottom(b)],
      ]) {
        if (
          Math.abs(shift) > distance ||
          (best[axis] !== null && Math.abs(shift) >= Math.abs(best[axis]))
        )
          continue;
        if (touchesAcross(axis, a, b, shift)) best[axis] = shift;
      }
    }
  for (const delta of [
    [best.x ?? 0, best.y ?? 0],
    [best.x ?? 0, 0],
    [0, best.y ?? 0],
  ]) {
    if (delta[0] === 0 && delta[1] === 0) continue;
    const snapped = movePlacement(groups, placement, moving, delta);
    if (arrangementGeometry(groups, snapped).valid) return snapped;
  }
  return placement;
}

function touchesAcross(axis, a, b, shift) {
  const moved = axis === "x" ? { ...b, x: b.x + shift } : { ...b, y: b.y + shift };
  return axis === "x"
    ? Math.min(bottom(a), bottom(moved)) - Math.max(a.y, moved.y) > EPSILON
    : Math.min(right(a), right(moved)) - Math.max(a.x, moved.x) > EPSILON;
}

// A drop only commits where it is legal: grouped needs a real crossing, free needs no overlap.
export function resolvePlacement(groups, placement, moving) {
  if (!isPlacement(groups, placement)) return null;
  const geometry = arrangementGeometry(groups, placement);
  const acceptable = (g) => g.valid && (placement.mode === "free" || g.connected);
  if (acceptable(geometry)) return placement;
  const ids = new Set(movingIds(groups, placement, moving));
  if (!ids.size) return null;
  const all = tiles(groups, placement);
  const carried = all.filter((t) => ids.has(t.id));
  const fixed = all.filter((t) => !ids.has(t.id));
  const seen = new Set();
  const candidates = [];
  const add = (dx, dy) => {
    const key = `${dx}|${dy}`;
    if (seen.has(key) || !Number.isFinite(dx) || !Number.isFinite(dy)) return;
    seen.add(key);
    candidates.push([dx, dy]);
  };
  for (const a of fixed)
    for (const b of carried) {
      const slideY = slides(0, a.y, a.height, b.y, b.height);
      const slideX = slides(0, a.x, a.width, b.x, b.width);
      for (const dx of [right(a) - b.x, a.x - right(b)]) for (const dy of slideY) add(dx, dy);
      for (const dy of [bottom(a) - b.y, a.y - bottom(b)]) for (const dx of slideX) add(dx, dy);
    }
  const eligible = candidates
    .map((delta) => ({ delta, placement: movePlacement(groups, placement, moving, delta) }))
    .filter(
      (c) =>
        isPlacement(groups, c.placement) && acceptable(arrangementGeometry(groups, c.placement)),
    );
  if (!eligible.length) return null;
  eligible.sort((a, b) => Math.hypot(...a.delta) - Math.hypot(...b.delta));
  return eligible[0].placement;
}

// Deltas along a seam: keep the current slide where it still overlaps, else align or centre the edges.
function slides(requested, aStart, aSpan, bStart, bSpan) {
  const margin = Math.min(MIN_CONTACT, aSpan / 2, bSpan / 2);
  return [
    clamp(requested, aStart - bStart - bSpan + margin, aStart + aSpan - bStart - margin),
    aStart - bStart,
    aStart + aSpan - bStart - bSpan,
    aStart + aSpan / 2 - bStart - bSpan / 2,
  ];
}

export function placeGroup(groups, side = "right") {
  if (!groups || !Object.hasOwn(OPPOSITE, side)) return null;
  const horizontal = side === "left" || side === "right";
  const reference = horizontal
    ? [
        side === "right" ? groups.source.width : -groups.destination.width,
        (groups.source.height - groups.destination.height) / 2,
      ]
    : [
        (groups.source.width - groups.destination.width) / 2,
        side === "bottom" ? groups.source.height : -groups.destination.height,
      ];
  const candidates = [reference];
  for (const a of groups.source.displays)
    for (const b of groups.destination.displays) {
      candidates.push(
        horizontal
          ? [
              side === "right" ? right(a) - b.x : a.x - right(b),
              a.y + a.height / 2 - b.y - b.height / 2,
            ]
          : [
              a.x + a.width / 2 - b.x - b.width / 2,
              side === "bottom" ? bottom(a) - b.y : a.y - bottom(b),
            ],
      );
    }
  const eligible = candidates.filter(
    (candidate) => arrangementGeometry(groups, groupedPlacement(groups, candidate)).connected,
  );
  eligible.sort((a, b) => separation(a, reference) - separation(b, reference));
  return groupedPlacement(groups, eligible[0] ?? reference);
}

// --- saved layouts -------------------------------------------------------

// Recover a saved grouped translation only when every recorded segment matches that geometry.
export function placementFromCrossings(groups, crossings) {
  if (!groups || !Array.isArray(crossings) || !crossings.length) return null;
  const first = crossings[0];
  if (OPPOSITE[first.fromEdge] !== first.toEdge) return null;
  const a = groups.source.displays.find((d) => d.id === first.fromDisplay);
  const b = groups.destination.displays.find((d) => d.id === first.toDisplay);
  if (!a || !b) return null;
  const fromSpan = first.fromSpan ?? [0, 1];
  const toSpan = first.toSpan ?? [0, 1];
  const vertical = first.fromEdge === "left" || first.fromEdge === "right";
  const offset = vertical
    ? [
        (first.fromEdge === "right" ? right(a) : a.x) - (first.toEdge === "right" ? right(b) : b.x),
        a.y + a.height * fromSpan[0] - b.y - b.height * toSpan[0],
      ]
    : [
        a.x + a.width * fromSpan[0] - b.x - b.width * toSpan[0],
        (first.fromEdge === "bottom" ? bottom(a) : a.y) -
          (first.toEdge === "bottom" ? bottom(b) : b.y),
      ];
  const placement = groupedPlacement(groups, offset);
  return matchesCrossings(groups, placement, crossings) ? placement : null;
}

export function matchesCrossings(groups, placement, crossings) {
  const geometry = arrangementGeometry(groups, placement);
  if (!geometry.connected || geometry.seams.length !== crossings.length) return false;
  return crossings.every((c) => geometry.seams.some((s) => sameSeam(c, s)));
}

// Saved positions win over crossings; a grouped save may omit positions and still reconstruct.
export function placementFromLayout(groups, arrangement, crossings) {
  if (!groups) return null;
  const saved =
    arrangement && MODES.includes(arrangement.mode) && Array.isArray(arrangement.positions)
      ? arrangement
      : null;
  if (saved) {
    const positions = {};
    for (const entry of saved.positions) {
      if (
        !entry ||
        typeof entry.display !== "string" ||
        !point([entry.x, entry.y]) ||
        positions[entry.display]
      )
        return null;
      positions[entry.display] = [entry.x, entry.y];
    }
    const placement = { mode: saved.mode, positions };
    if (isPlacement(groups, placement)) {
      if (saved.mode === "grouped" && !placementOffset(groups, placement)) return null;
      return !crossings || matchesCrossings(groups, placement, crossings) ? placement : null;
    }
    if (saved.mode === "free") return null;
  }
  return placementFromCrossings(groups, crossings);
}

export function layoutArrangement(placement, hidden = []) {
  if (!placement) return null;
  const positions = Object.entries(placement.positions)
    .toSorted(([a], [b]) => a.localeCompare(b))
    .map(([display, [x, y]]) => ({ display, x, y }));
  // Always listed, even empty: a layout that shows every display must restore that way.
  return { mode: placement.mode, positions, hidden: hidden.toSorted((a, b) => a.localeCompare(b)) };
}

// --- plain-language description -----------------------------------------

export function describeArrangement(arrangement) {
  if (!arrangement?.connected || !arrangement.seams?.length) return arrangement?.message ?? "";
  const [first] = arrangement.seams;
  const from = displayName(arrangement.groups?.source, first.fromDisplay);
  const to = displayName(arrangement.groups?.destination, first.toDisplay);
  return arrangement.seams.length === 1
    ? `The pointer crosses on the ${first.fromEdge} edge of ${from}, into ${to}.`
    : `The pointer crosses on ${arrangement.seams.length} edges, starting at the ${first.fromEdge} edge of ${from}.`;
}

function displayName(sideGroup, id) {
  return sideGroup?.displays.find((d) => d.id === id)?.name || "that display";
}

export function formatSize(width, height) {
  return `${Math.round(width)} × ${Math.round(height)}`;
}

export function spokenSize(width, height) {
  return `${Math.round(width)} by ${Math.round(height)}`;
}

// --- canvas fitting ------------------------------------------------------

function arrangementBounds(all) {
  const x = Math.min(...all.map((t) => t.x));
  const y = Math.min(...all.map((t) => t.y));
  return {
    x,
    y,
    width: Math.max(1, Math.max(...all.map(right)) - x),
    height: Math.max(1, Math.max(...all.map(bottom)) - y),
  };
}

// Insets keep a label band above and below the displays, so no label is drawn off the canvas.
export function innerBox(stage, insets = VIEW_INSETS) {
  return {
    x: insets.left,
    y: insets.top,
    width: Math.max(1, stage.width - insets.left - insets.right),
    height: Math.max(1, stage.height - insets.top - insets.bottom),
  };
}

export function fitTransform(all, stage, insets = VIEW_INSETS) {
  const bounds = arrangementBounds(all);
  const box = innerBox(stage, insets);
  const scale = Math.min(box.width / bounds.width, box.height / bounds.height);
  return {
    scale,
    originX: box.x + (box.width - bounds.width * scale) / 2 - bounds.x * scale,
    originY: box.y + (box.height - bounds.height * scale) / 2 - bounds.y * scale,
  };
}

export function tileRects(all, transform) {
  const rects = {};
  for (const t of all)
    rects[t.id] = {
      x: transform.originX + t.x * transform.scale,
      y: transform.originY + t.y * transform.scale,
      width: t.width * transform.scale,
      height: t.height * transform.scale,
    };
  return rects;
}

// The box around each computer's displays, where its label and its group drag live.
export function sideRects(all, transform) {
  const rects = {};
  for (const side of SIDES) {
    const own = all.filter((t) => t.side === side);
    if (!own.length) continue;
    const bounds = arrangementBounds(own);
    rects[side] = {
      x: transform.originX + bounds.x * transform.scale,
      y: transform.originY + bounds.y * transform.scale,
      width: bounds.width * transform.scale,
      height: bounds.height * transform.scale,
    };
  }
  return rects;
}

// Keep the scale a nudge was made at and only slide the view, so the canvas never rescales under an edit.
export function constrainTransform(all, transform, stage, insets = VIEW_INSETS) {
  if (!transform || !Number.isFinite(transform.scale) || transform.scale <= 0) return null;
  const box = innerBox(stage, insets);
  const bounds = arrangementBounds(all);
  const width = bounds.width * transform.scale;
  const height = bounds.height * transform.scale;
  if (width > box.width + 1 || height > box.height + 1) return null;
  return {
    scale: transform.scale,
    originX: clamp(
      transform.originX,
      box.x - bounds.x * transform.scale,
      box.x + box.width - width - bounds.x * transform.scale,
    ),
    originY: clamp(
      transform.originY,
      box.y - bounds.y * transform.scale,
      box.y + box.height - height - bounds.y * transform.scale,
    ),
  };
}

export function transformFits(all, transform, stage, insets = VIEW_INSETS) {
  if (!transform || !Number.isFinite(transform.scale) || transform.scale <= 0) return false;
  const box = innerBox(stage, insets);
  return Object.values(tileRects(all, transform)).every(
    (rect) =>
      rect.x >= box.x - 1 &&
      rect.y >= box.y - 1 &&
      rect.x + rect.width <= box.x + box.width + 1 &&
      rect.y + rect.height <= box.y + box.height + 1,
  );
}

export function sameTransform(a, b) {
  return (
    Boolean(a && b) &&
    Math.abs(a.scale - b.scale) <= Math.max(a.scale, b.scale) * 1e-3 &&
    Math.abs(a.originX - b.originX) <= 0.5 &&
    Math.abs(a.originY - b.originY) <= 0.5
  );
}

// --- labels --------------------------------------------------------------

// Above the group when that band is free, otherwise below it, otherwise tucked inside on a plate.
export function labelPlacement(self, other, stage, size, insets = VIEW_INSETS, avoid = []) {
  const x = clamp(
    self.x,
    insets.left,
    Math.max(insets.left, stage.width - insets.right - size.width),
  );
  const blocked = [other, ...avoid].filter(Boolean);
  const candidates = [
    { x, y: self.y - LABEL_GAP - size.height, placement: "above" },
    { x, y: self.y + self.height + LABEL_GAP, placement: "below" },
  ];
  for (const candidate of candidates) {
    const box = { ...candidate, width: size.width, height: size.height };
    if (box.y < 0 || box.y + box.height > stage.height) continue;
    if (blocked.some((rect) => intersects(box, rect))) continue;
    return candidate;
  }
  return { x: self.x + LABEL_GAP, y: self.y + LABEL_GAP, placement: "inside" };
}

export function truncateToWidth(value, maxWidth, measure) {
  const characters = Array.from(String(value ?? ""));
  if (!characters.length || !Number.isFinite(maxWidth) || maxWidth <= 0) return "";
  if (measure(characters.join("")) <= maxWidth) return characters.join("");
  let low = 0;
  let high = characters.length - 1;
  while (low < high) {
    const middle = Math.ceil((low + high) / 2);
    if (measure(`${characters.slice(0, middle).join("")}…`) <= maxWidth) low = middle;
    else high = middle - 1;
  }
  return low > 0 ? `${characters.slice(0, low).join("")}…` : "";
}

function intersects(a, b) {
  return (
    Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x) > 0 &&
    Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y) > 0
  );
}

function right(d) {
  return d.x + d.width;
}
function bottom(d) {
  return d.y + d.height;
}
function overlaps(a, b) {
  return (
    Math.min(right(a), right(b)) - Math.max(a.x, b.x) > EPSILON &&
    Math.min(bottom(a), bottom(b)) - Math.max(a.y, b.y) > EPSILON
  );
}
function near(a, b) {
  return Math.abs(a - b) <= EPSILON;
}
function point(value) {
  return (
    Array.isArray(value) &&
    value.length === 2 &&
    value.every((n) => Number.isFinite(n) && Math.abs(n) <= MAX_COORDINATE)
  );
}
function separation(a, b) {
  return Math.hypot(a[0] - b[0], a[1] - b[1]);
}
function clamp(value, minimum, maximum) {
  return Math.min(Math.max(value, minimum), Math.max(minimum, maximum));
}
