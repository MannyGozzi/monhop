import {
  arrangementGeometry,
  chainPlacement,
  computerGroups,
  drawnDisplaysAcross,
  formatSize,
  groupRects,
  hiddenDisplaysAcross,
  hiddenFromLayout,
  monitorKey,
  placementFromLayout,
  sameTransform,
  sharedMonitorsAcross,
  tileRects,
} from "./arrangement-model.mjs";
import {
  createGroupLabelNode,
  createGroupNode,
  createLegend,
  createRulerNode,
  createSeamLayer,
  tileMarks,
  createSlotNode,
  createTilePaint,
  labelBoxes,
  matrixOf,
  refineText,
  svgNode,
} from "./arrangement-render.mjs";
import { cappedFit, heldShift } from "./arrangement-view-model.mjs";
import { normalizeStoredLayout } from "./sharing-model.mjs";

const MAX_U64 = "18446744073709551615";
const MAX_COORDINATE = 20_000_000;
const STALE = "Saved arrangement details need a fresh review.";
// Below this a move is a rounding difference, not a rearrangement worth animating.
const MOVE_EPSILON = 0.5;
// The room the canvas keeps around its picture: a label band above, the ruler and the legend strip
// below, and air at the sides. Each grows with the canvas past its floor.
export const CANVAS_MARGIN = Object.freeze({
  top: 52,
  side: 40,
  bottom: 48,
  ruler: 34,
  share: 0.09,
});
// Labels may reach past the picture's margin, never past these.
const LABEL_INSETS = Object.freeze({ top: 0, right: 12, bottom: 0, left: 12 });
const RULER_GAP = 18;
// A card under a picture: the label band above the picture, the gap down to the card, and the
// least room kept above the label.
const COMPOSE = Object.freeze({ label: 24, gap: 28, edge: 16 });
const MIN_STAGE = { width: 240, height: 160 };
// A lone computer's displays sit side by side with this much of the widest one between them, so
// they read as one computer's screens without claiming an arrangement nobody made.
const LOCAL_GAP = 0.06;
// The slot for a computer still pairing: a typical display of the arrangement, 16:10, beside it.
const SLOT_ASPECT = 10 / 16;
const SLOT_GAP = 0.2;
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
  return DRAWN_DISPLAYS_CAPTION;
}

// The diagram follows this computer's displays live and the other computer's as last reported.
export const DRAWN_DISPLAYS_NOTE = "Displays as each computer last reported them.";
// The same, short enough for the legend strip under the picture.
export const DRAWN_DISPLAYS_CAPTION = "Sizes as each computer last reported";

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

// --- fitting the canvas ------------------------------------------------------

// The picture's margins for a canvas of `stage`'s size. `reserve` is room a card below the picture
// takes; it never takes more than half the canvas, so the picture always keeps some.
export function canvasInsets(stage, { reserve = 0, ruler = false } = {}) {
  const side = Math.max(CANVAS_MARGIN.side, stage.width * CANVAS_MARGIN.share);
  const top = Math.max(CANVAS_MARGIN.top, stage.height * CANVAS_MARGIN.share);
  const bottom =
    Math.max(CANVAS_MARGIN.bottom, stage.height * CANVAS_MARGIN.share) +
    (ruler ? CANVAS_MARGIN.ruler : 0) +
    Math.min(Math.max(0, reserve), stage.height / 2);
  return { top, right: side, bottom, left: side };
}

// With a card under the picture the two are centred as one block: the label band, the picture and,
// `CARD_GAP` below it, the card `card` pixels tall. Returns the moved fit and where the card's top lands.
export function composeWithCard(fit, extent, stage, card) {
  if (!(card > 0) || !extent) return { fit, cardTop: null };
  const block = COMPOSE.label + extent.height + COMPOSE.gap + card;
  const start = Math.max(COMPOSE.edge, (stage.height - block) / 2);
  const shift = start + COMPOSE.label - extent.y;
  return {
    fit: { ...fit, originY: fit.originY + shift },
    cardTop: Math.round(start + COMPOSE.label + extent.height + COMPOSE.gap),
  };
}

// This computer's displays as the snapshot reports them: sizes but no positions, so they stand
// side by side on one baseline, in the order reported.
export function localDisplayTiles(reported) {
  const list = (Array.isArray(reported) ? reported : [])
    .filter((display) => display?.width > 1 && display?.height > 1)
    .slice(0, 16);
  if (!list.length) return [];
  const widest = Math.max(...list.map((display) => display.width));
  const tallest = Math.max(...list.map((display) => display.height));
  let x = 0;
  return list.map((display, index) => {
    const tile = {
      id: `local-${index}`,
      name: typeof display.name === "string" && display.name ? display.name : "Display",
      width: display.width,
      height: display.height,
      x,
      y: tallest - display.height,
      primary: display.primary === true,
      group: "local",
      side: "local",
    };
    x += display.width + widest * LOCAL_GAP;
    return tile;
  });
}

function boundsOf(rects) {
  const list = Object.values(rects ?? {});
  if (!list.length) return null;
  const x = Math.min(...list.map((rect) => rect.x));
  const y = Math.min(...list.map((rect) => rect.y));
  return {
    x,
    y,
    width: Math.max(...list.map((rect) => rect.x + rect.width)) - x,
    height: Math.max(...list.map((rect) => rect.y + rect.height)) - y,
  };
}

// Where a computer that is still pairing will go: a typical display's size, beside the others.
export function slotTile(tiles) {
  const bounds = boundsOf(tiles);
  if (!bounds) return { id: "slot", x: 0, y: 0, width: 2560, height: 2560 * SLOT_ASPECT };
  const width = tiles.reduce((sum, tile) => sum + tile.width, 0) / tiles.length;
  const height = width * SLOT_ASPECT;
  return {
    id: "slot",
    x: bounds.x + bounds.width + width * SLOT_GAP,
    y: bounds.y + bounds.height - height,
    width,
    height,
  };
}

// The whole arrangement's size in desktop pixels, as the ruler under it reads.
export function extentLabel(tiles) {
  const bounds = boundsOf(tiles);
  return bounds ? formatSize(bounds.width, bounds.height) : "";
}

// Where to start a refitted picture so it first draws exactly where the old fit drew it: the old
// drawing's own glide (`visual`, a uniform scale and offset) composed with the step from the new
// fit back to the old one. Easing that to nothing glides the picture to its new fit.
export function carryTransform(visual, from, to) {
  const scale = visual?.scale ?? 1;
  const k = from.scale / to.scale;
  return {
    scale: scale * k,
    x: scale * (from.originX - k * to.originX) + (visual?.x ?? 0),
    y: scale * (from.originY - k * to.originY) + (visual?.y ?? 0),
  };
}

export function restingCarry(carry) {
  return Math.abs(carry.scale - 1) < 1e-3 && Math.abs(carry.x) < 0.5 && Math.abs(carry.y) < 0.5;
}

// The transform that draws `next` where `drawn` was, about `next`'s own top-left corner.
export function flipFrom(drawn, next) {
  return {
    x: drawn.x - next.x,
    y: drawn.y - next.y,
    scaleX: drawn.width / Math.max(1e-6, next.width),
    scaleY: drawn.height / Math.max(1e-6, next.height),
  };
}

function restingFlip(flip) {
  return (
    Math.abs(flip.x) < MOVE_EPSILON &&
    Math.abs(flip.y) < MOVE_EPSILON &&
    Math.abs(flip.scaleX - 1) < 1e-3 &&
    Math.abs(flip.scaleY - 1) < 1e-3
  );
}

function modelSignature(tiles, seams) {
  return JSON.stringify([
    tiles.map((tile) => [tile.id, tile.group, tile.x, tile.y, tile.width, tile.height]),
    seams.map((seam) => [seam.start, seam.end]),
  ]);
}

// --- the canvas picture ---------------------------------------------------------

// The saved arrangement filling the canvas. Returns `{element, refit(stage)}`: the canvas calls
// refit whenever its own size changes, and the picture glides to the new fit.
export function createDashboardArrangement(setup, names = {}) {
  const arrangement = savedDashboardArrangement(setup);
  if (!arrangement.available) {
    // Nothing is drawn, so the remembered positions would only make the next drawing slide in
    // from where a different arrangement once sat.
    if (names.motionKey) lastDrawn.delete(names.motionKey);
    const root = document.createElement("figure");
    root.className = "dashboard-arrangement-preview";
    root.dataset.state = "unavailable";
    const message = document.createElement("p");
    message.className = "dashboard-arrangement-unavailable";
    message.textContent = arrangement.message;
    root.append(message);
    return { element: root, hold() {}, refit() {}, update() {}, setCard() {}, cardTop: () => null };
  }
  const members = memberPresentation(names, arrangement.groups.order);
  const seamCount = arrangement.seams.length;
  return createPicture(
    {
      tiles: arrangement.tiles,
      seams: arrangement.seams,
      members,
      ruler: true,
      ariaLabel: `${joinLabels(members.map((member) => member.label))}. ${arrangement.noCrossingYet ? "No crossing yet." : `${seamCount} saved display seam${seamCount === 1 ? "" : "s"}.`}`,
      legend: [
        ...(arrangement.tiles.some((tile) => tile.primary)
          ? [{ kind: "primary", label: "Primary display" }]
          : []),
        ...(arrangement.tiles.some((tile) => tile.shared)
          ? [{ kind: "shared", label: "Cabled to both" }]
          : []),
        ...(seamCount ? [{ kind: "seam", label: "Pointer crossing" }] : []),
        { kind: "caption", label: dashboardCaption(arrangement) },
      ],
    },
    names,
  );
}

// This computer's own displays, for a canvas with no arrangement to show yet. Null with none known.
export function createLocalPicture(reported, names = {}) {
  const tiles = localDisplayTiles(reported);
  if (!tiles.length) {
    if (names.motionKey) lastDrawn.delete(names.motionKey);
    return null;
  }
  const name = label(names.local, "This computer");
  return createPicture(
    {
      tiles,
      seams: [],
      members: [
        {
          key: "local",
          label: name,
          platform: platform(names.localPlatform, "macos"),
          tone: "local",
        },
      ],
      ruler: false,
      ariaLabel: `${name}'s displays`,
      legend: tiles.some((tile) => tile.primary)
        ? [{ kind: "primary", label: "Primary display" }]
        : [],
    },
    names,
  );
}

// `names.statuses`, keyed by member, gives each label its state dot and words, and marks a computer
// that cannot be reached as offline. `names.quiet` lists members drawn quietly, and `names.slot`
// ({title, detail}) adds the dashed place a computer still pairing will take. `names.motion`
// carries dom.mjs's motion helpers; without it the picture never moves.
function createPicture(content, names) {
  const root = document.createElement("figure");
  root.className = "dashboard-arrangement-preview";
  root.dataset.size = "canvas";
  root.dataset.state = "saved";
  const svg = svgNode("svg");
  svg.classList.add("dashboard-arrangement-canvas");
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", content.ariaLabel);
  const paint = createTilePaint();
  const scene = svgNode("g");
  scene.classList.add("arrangement-scene");
  svg.append(paint.defs, scene);
  root.append(svg);
  if (content.legend.length) root.append(createLegend(content.legend, { tag: "figcaption" }));

  const motion = names.motion ?? null;
  const key = names.motionKey ?? null;
  const labels = Object.fromEntries(content.members.map((member) => [member.key, member.label]));
  const model = modelSignature(content.tiles, content.seams);
  let looks = pictureLooks(content, names);
  let slotCopy = names.slot ?? null;
  let card = names.card ?? 0;
  let drawn = null;

  // Where the picture and its card sit on a canvas of `size`, without drawing anything.
  function placeFor(size) {
    const slot = slotCopy ? slotTile(content.tiles) : null;
    const fitted = slot ? [...content.tiles, slot] : content.tiles;
    const insets = canvasInsets(size, {
      reserve: card > 0 ? card + COMPOSE.gap : 0,
      ruler: content.ruler,
    });
    const capped = cappedFit(fitted, size, insets);
    const place = composeWithCard(capped, boundsOf(tileRects(fitted, capped)), size, card);
    return { slot, fitted, ...place };
  }

  // While the canvas glides the drawing keeps its scale and follows the new fit's centre by viewBox
  // alone; the refit once the canvas settles glides on from where it is held.
  function hold(stage) {
    if (!drawn) return;
    const size = canvasSize(stage);
    const { fitted, fit } = placeFor(size);
    const { x, y } = heldShift(drawn.fit, fit, boundsOf(fitted));
    drawn.hold = Math.abs(x) < MOVE_EPSILON && Math.abs(y) < MOVE_EPSILON ? null : { size, x, y };
    const origin = drawn.hold ? `${-x} ${-y}` : "0 0";
    svg.setAttribute("viewBox", `${origin} ${size.width} ${size.height}`);
  }

  function draw(stage, { force = false, still = false } = {}) {
    const size = canvasSize(stage);
    const same = drawn?.size.width === size.width && drawn?.size.height === size.height;
    if (!force && same && !drawn.hold) return;
    const previous = key ? lastDrawn.get(key) : drawn;
    const visual = previous?.scene?.isConnected ? visualOf(previous.scene, previous.hold) : null;
    svg.setAttribute("viewBox", `0 0 ${size.width} ${size.height}`);
    root.dataset.slot = String(Boolean(slotCopy));
    const { slot, fit, cardTop } = placeFor(size);
    const rects = tileRects(content.tiles, fit);
    const groups = groupRects(content.tiles, fit);
    const boxes = labelBoxes(groups, labels, size, LABEL_INSETS, looks.details);
    const groupsLayer = svgNode("g");
    groupsLayer.classList.add("arrangement-groups");
    for (const member of content.members) {
      if (!groups[member.key]) continue;
      groupsLayer.append(
        createGroupNode({
          tiles: content.tiles.filter((tile) => (tile.group ?? tile.side) === member.key),
          tileRects: rects,
          groupKey: member.key,
          platform: member.platform,
          side: member.key,
          tone: member.tone,
          label: member.label,
          rect: groups[member.key],
          labelBox: boxes[member.key],
          paint,
          state: looks.offline.has(member.key) ? "offline" : null,
          quiet: looks.quiet.has(member.key),
        }),
      );
    }
    const seams = createSeamLayer(content.seams, fit, {
      offTiles: looks.offTiles,
      keepClear: tileMarks(content.tiles, rects),
    });
    seams.dataset.motionId = "seams";
    const extent = boundsOf(rects);
    const ruler =
      content.ruler && extent
        ? createRulerNode(
            { x: extent.x, y: extent.y + extent.height + RULER_GAP, width: extent.width },
            extentLabel(content.tiles),
          )
        : null;
    if (ruler) ruler.dataset.motionId = "ruler";
    const slotNode = slot ? slotNodeFor(slot, fit, slotCopy, motion) : null;
    scene.replaceChildren(groupsLayer, seams, ...[slotNode, ruler].filter(Boolean));
    const state = {
      size,
      fit,
      cardTop,
      model,
      rects,
      groups,
      boxes,
      slot,
      slotCopy,
      scene,
      animations: [],
    };
    playMotion(previous, visual, state, still ? null : motion);
    drawn = state;
    if (key) lastDrawn.set(key, state);
    // Real text metrics need a laid-out canvas, so the estimated truncation is corrected on the next frame.
    if (typeof requestAnimationFrame === "function") requestAnimationFrame(() => refineText(svg));
  }

  // A poll that changes only a computer's state restyles the nodes in place, so CSS eases the
  // change; a slot coming or going changes the fit, so the picture is redrawn and glides.
  function update(next = {}) {
    const nextSlot = next.slot ?? null;
    looks = pictureLooks(content, next);
    if (!drawn) return;
    if (JSON.stringify(nextSlot) !== JSON.stringify(slotCopy)) {
      slotCopy = nextSlot;
      draw(drawn.hold?.size ?? drawn.size, { force: true });
      return;
    }
    restyle();
  }

  function restyle() {
    const boxes = labelBoxes(drawn.groups, labels, drawn.size, LABEL_INSETS, looks.details);
    for (const node of scene.querySelectorAll(".arrangement-group")) {
      const member = node.dataset.group;
      setData(node, "state", looks.offline.has(member) ? "offline" : null);
      setData(node, "quiet", looks.quiet.has(member) ? "true" : null);
      const shownLabel = node.querySelector(".arrangement-group-label");
      const was = drawn.boxes[member];
      const box = boxes[member];
      if (!shownLabel || !box) continue;
      if (sameLabelPlace(was, box)) {
        const dot = shownLabel.querySelector(".arrangement-group-label-dot");
        if (dot && box.detail?.tone) dot.dataset.tone = box.detail.tone;
        continue;
      }
      const replacement = createGroupLabelNode(labels[member], box, drawn.groups[member]);
      replacement.dataset.motionId = `label:${member}`;
      shownLabel.replaceWith(replacement);
      if (motion?.enabled())
        replacement.animate([{ opacity: 0 }, { opacity: 1 }], {
          duration: motion.ms("--motion-base"),
          easing: motion.ease("--ease-out"),
        });
    }
    drawn.boxes = boxes;
    for (const node of scene.querySelectorAll(".arrangement-seam-run, .arrangement-seam-handle"))
      setData(
        node,
        "tone",
        looks.offTiles.has(node.dataset.from) || looks.offTiles.has(node.dataset.to) ? "off" : null,
      );
  }

  // The card's height is known only once it is laid out; a picture not yet on screen takes it still.
  function setCard(height, { still = false } = {}) {
    if (!drawn || Math.abs(height - card) < 1) return;
    card = height;
    draw(drawn.hold?.size ?? drawn.size, { force: true, still });
  }

  draw(names.stage);
  return {
    element: root,
    hold,
    refit: (stage) => draw(stage),
    update,
    setCard,
    cardTop: () =>
      drawn && drawn.cardTop !== null ? Math.round(drawn.cardTop + (drawn.hold?.y ?? 0)) : null,
  };
}

function canvasSize(stage) {
  return {
    width: Math.max(MIN_STAGE.width, Math.round(stage?.width || MIN_STAGE.width)),
    height: Math.max(MIN_STAGE.height, Math.round(stage?.height || MIN_STAGE.height)),
  };
}

// How each computer looks: its label's dot and words, offline or not, quiet or not.
function pictureLooks(content, names) {
  const statuses = names.statuses ?? {};
  const offline = new Set(
    content.members.filter((member) => statuses[member.key]?.offline).map((member) => member.key),
  );
  return {
    offline,
    quiet: new Set(names.quiet ?? []),
    offTiles: new Set(
      content.tiles.filter((tile) => offline.has(tile.group ?? tile.side)).map((tile) => tile.id),
    ),
    details: Object.fromEntries(
      content.members.map((member) => [
        member.key,
        statuses[member.key]
          ? { tone: statuses[member.key].tone, status: statuses[member.key].label ?? "" }
          : null,
      ]),
    ),
  };
}

function sameLabelPlace(a, b) {
  return (
    Boolean(a && b) &&
    a.placement === b.placement &&
    Math.abs(a.x - b.x) < 0.5 &&
    Math.abs(a.y - b.y) < 0.5 &&
    Math.abs(a.width - b.width) < 0.5 &&
    (a.detail?.status ?? "") === (b.detail?.status ?? "") &&
    Boolean(a.detail?.tone) === Boolean(b.detail?.tone)
  );
}

function setData(node, name, value) {
  if (value === null) delete node.dataset[name];
  else if (node.dataset[name] !== value) node.dataset[name] = value;
}

function slotNodeFor(slot, fit, copy, motion) {
  const node = createSlotNode(tileRects([slot], fit).slot, copy);
  node.dataset.motionId = "slot";
  // The slot breathes on the document clock, so a redrawn slot never restarts it.
  const period = motion?.ms("--canvas-slot-loop") ?? 0;
  if (period > 0)
    node.style.setProperty("--canvas-slot-phase", `${-Math.round(performance.now() % period)}ms`);
  return node;
}

// What a drawing shows right now: its scene's glide, moved by any hold, and each display's own,
// mid-flight or not.
function visualOf(scene, hold) {
  const tiles = {};
  for (const node of scene.querySelectorAll(".arrangement-display")) {
    const matrix = matrixOf(node);
    if (matrix) tiles[node.dataset.display] = matrix;
  }
  const glide = matrixOf(scene);
  if (!hold) return { scene: glide, tiles };
  const base = glide ?? { scale: 1, scaleY: 1, x: 0, y: 0 };
  return { scene: { ...base, x: base.x + hold.x, y: base.y + hold.y }, tiles };
}

// One drawing replaces another three ways. The same arrangement at the same fit carries on any
// motion still running. The same arrangement refitted glides as one picture from where it is drawn.
// A changed arrangement glides each display from where it was drawn, and fades in the rest.
function playMotion(previous, visual, state, motion) {
  const sceneMotion = () => state.scene.getAnimations?.() ?? [];
  if (!motion?.enabled()) {
    for (const animation of sceneMotion()) animation.cancel();
    return;
  }
  if (!previous || !visual) return;
  const animate = (node, id, frames, duration, easing) => {
    state.animations.push({
      id,
      animation: node.animate(frames, {
        duration: motion.ms(duration),
        easing: motion.ease(easing),
      }),
    });
  };
  const sameModel = previous.model === state.model;
  if (sameModel && sameTransform(previous.fit, state.fit) && !previous.hold) {
    if (previous.scene === state.scene) return;
    for (const { id, animation } of previous.animations) {
      if (animation.playState !== "running") continue;
      const node =
        id === "scene" ? state.scene : state.scene.querySelector(`[data-motion-id="${id}"]`);
      if (node) state.animations.push({ id, animation: motion.copy(animation, node) });
    }
    return;
  }
  for (const animation of sceneMotion()) animation.cancel();
  if (sameModel) {
    const start = carryTransform(visual.scene, previous.fit, state.fit);
    if (!restingCarry(start))
      animate(
        state.scene,
        "scene",
        [
          { transform: `matrix(${start.scale}, 0, 0, ${start.scale}, ${start.x}, ${start.y})` },
          { transform: "none" },
        ],
        "--motion-slow",
        "--ease-out",
      );
    slotMotion(previous, state, animate, motion);
    return;
  }
  const scene = visual.scene ?? { scale: 1, x: 0, y: 0 };
  for (const [id, rect] of Object.entries(state.rects)) {
    const node = state.scene.querySelector(`[data-motion-id="tile:${id}"]`);
    if (!node) continue;
    const was = previous.rects[id];
    if (!was) {
      animate(node, `tile:${id}`, enterFrames(rect, motion), "--motion-slow", "--ease-out");
      continue;
    }
    const own = visual.tiles[id] ?? { scale: 1, scaleY: 1, x: 0, y: 0 };
    const drawn = {
      x: scene.scale * (was.x + own.x) + scene.x,
      y: scene.scale * (was.y + own.y) + scene.y,
      width: scene.scale * own.scale * was.width,
      height: scene.scale * own.scaleY * was.height,
    };
    const flip = flipFrom(drawn, rect);
    if (restingFlip(flip)) continue;
    animate(
      node,
      `tile:${id}`,
      [
        { transform: `translate(${flip.x}px, ${flip.y}px) scale(${flip.scaleX}, ${flip.scaleY})` },
        { transform: "none" },
      ],
      "--motion-spring",
      "--ease-spring",
    );
  }
  for (const node of state.scene.querySelectorAll(
    '[data-motion-id^="label:"], [data-motion-id="seams"], [data-motion-id="ruler"], [data-motion-id="slot"]',
  ))
    animate(
      node,
      node.dataset.motionId,
      [{ opacity: 0 }, { opacity: 1 }],
      "--motion-slow",
      "--ease-out",
    );
}

// A slot that arrives fades in; one that leaves fades out where the glide carries it, starting
// where it was drawn: its old place, mapped into the new fit, is exactly where the glide begins.
function slotMotion(previous, state, animate, motion) {
  if (state.slot && !previous.slot) {
    const node = state.scene.querySelector('[data-motion-id="slot"]');
    if (node)
      animate(node, "slot", [{ opacity: 0 }, { opacity: 1 }], "--motion-slow", "--ease-out");
    return;
  }
  if (!previous.slot || state.slot || !previous.slotCopy) return;
  const ghost = createSlotNode(tileRects([previous.slot], state.fit).slot, previous.slotCopy);
  ghost.dataset.exit = "true";
  state.scene.append(ghost);
  const animation = ghost.animate([{ opacity: 1 }, { opacity: 0 }], {
    duration: motion.ms("--motion-base"),
    easing: motion.ease("--ease-out"),
    fill: "forwards",
  });
  animation.finished.then(
    () => ghost.remove(),
    () => ghost.remove(),
  );
}

// A display arriving grows into place about its own centre, from nearly full size.
function enterFrames(rect, motion) {
  const scale = Number.parseFloat(motion.token("--scale-enter")) || 1;
  const x = (rect.width * (1 - scale)) / 2;
  const y = (rect.height * (1 - scale)) / 2;
  return [
    { opacity: 0, transform: `translate(${x}px, ${y}px) scale(${scale})` },
    { opacity: 1, transform: "none" },
  ];
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
