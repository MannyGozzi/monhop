// One read-only drawing path for the arrangement editor and the canvas picture.
import { formatSize, labelPlacement, spokenSize, truncateToWidth } from "./arrangement-model.mjs";
import { icon } from "./icons.mjs";

const SVG_NS = "http://www.w3.org/2000/svg";
// The fonts text is measured with; studio-canvas.css draws it with the same families and weights.
const FAMILY =
  '-apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI Variable", "Segoe UI", ui-sans-serif, sans-serif';
const MONO = 'ui-monospace, "SF Mono", Menlo, Consolas, monospace';
const LABEL_FONT = `600 11.5px ${FAMILY}`;
const STATUS_FONT = `400 11.5px ${FAMILY}`;
const RULER_FONT = `500 10px ${MONO}`;
// A tile's type grows once the tile is roomy, so a lone display never reads as a speck of text.
const TILE_TYPE = {
  regular: { name: 11.5, size: 10, gap: 4 },
  large: { name: 14, size: 11.5, gap: 6 },
};
const LARGE_TILE = { width: 200, height: 112 };
const LABEL_HEIGHT = 16;
const LABEL_PLATE_PADDING = 6;
const LABEL_DOT = 6;
const LABEL_DOT_GAP = 6;
const STATUS_GAP = 6;
// A status word that would push a label this far past its displays drops to keep the name readable.
const LABEL_OVERHANG = 40;
const BEZEL = 2;
const TILE_RADIUS = 4.5;
const SCREEN_RADIUS = 2.5;
const TILE_PADDING = 8;
const MARKER_ROOM = 14;
const HANDLE_RADIUS = 10;
const HANDLE_MIN_SEAM = 30;
const HANDLE_STEP = 2;
const SLOT_PLUS = 10;
const SLOT_PLUS_GAP = 7;
const HANDLE_CLEARANCE = 3;
const RULER_TICK = 4;
const RULER_TEXT_GAP = 7;
const SLOT_RADIUS = 6;
// Lucide's arrow-left-right on its 24-unit grid, drawn at half size in the seam handle.
const SWAP_GLYPH = "M8 3 4 7l4 4M4 7h16M16 21l4-4-4-4M20 17H4";

let paintSequence = 0;

export function svgNode(name) {
  return document.createElementNS(SVG_NS, name);
}

export function setPosition(node, x, y) {
  node?.setAttribute("transform", `translate(${round(x)} ${round(y)})`);
}

// The gradients and hatch every tile of one drawing paints with. Ids are per drawing, so a picture
// fading out beside its replacement never borrows the other's paint.
export function createTilePaint() {
  paintSequence += 1;
  const id = `arrangement-paint-${paintSequence}`;
  const defs = svgNode("defs");
  defs.append(
    gradient(
      `${id}-depth`,
      [0, 0, 1, 1],
      [
        [0, "arrangement-paint-clear"],
        [1, "arrangement-paint-depth"],
      ],
    ),
    gradient(
      `${id}-sheen`,
      [0, 0, 0.9, 0.55],
      [
        [0, "arrangement-paint-sheen"],
        [0.45, "arrangement-paint-clear"],
      ],
    ),
    hatchPattern(`${id}-hatch`),
  );
  return {
    defs,
    depth: `url(#${id}-depth)`,
    sheen: `url(#${id}-sheen)`,
    hatch: `url(#${id}-hatch)`,
  };
}

function gradient(id, [x1, y1, x2, y2], stops) {
  const node = svgNode("linearGradient");
  node.id = id;
  for (const [name, value] of Object.entries({ x1, y1, x2, y2 }))
    node.setAttribute(name, String(value));
  for (const [offset, className] of stops) {
    const stop = svgNode("stop");
    stop.setAttribute("offset", String(offset));
    stop.classList.add(className);
    node.append(stop);
  }
  return node;
}

function hatchPattern(id) {
  const pattern = svgNode("pattern");
  pattern.id = id;
  pattern.setAttribute("patternUnits", "userSpaceOnUse");
  pattern.setAttribute("width", "5");
  pattern.setAttribute("height", "5");
  pattern.setAttribute("patternTransform", "rotate(45)");
  const line = svgNode("rect");
  line.classList.add("arrangement-paint-hatch");
  line.setAttribute("width", "1.2");
  line.setAttribute("height", "5");
  pattern.append(line);
  return pattern;
}

function labelSize(label, placement = "above", detail = null) {
  const width =
    (detail?.tone ? LABEL_DOT + LABEL_DOT_GAP : 0) +
    measureWith(LABEL_FONT)(label) +
    (detail?.status ? STATUS_GAP + measureWith(STATUS_FONT)(detail.status) : 0) +
    (placement === "inside" ? LABEL_PLATE_PADDING * 2 : 0);
  return { width: Math.ceil(width), height: LABEL_HEIGHT };
}

// Places every group's label in sequence, each one avoiding every other group's rect plus every
// label already placed, so N labels never land on top of a group or of each other. `details`, keyed
// like `labels`, adds a state dot and, while it fits, the state's words after the name.
export function labelBoxes(rects, labels, stage, insets, details = {}) {
  const keys = Object.keys(rects);
  const boxes = {};
  const placed = [];
  for (const key of keys) {
    const otherRects = keys.filter((k) => k !== key).map((k) => rects[k]);
    const detail = fittingDetail(rects[key], labels[key], details[key]);
    const next = box(rects[key], labels[key], stage, insets, [...otherRects, ...placed], detail);
    boxes[key] = { ...next, detail };
    placed.push(next);
  }
  return boxes;
}

function fittingDetail(rect, label, detail) {
  if (!detail?.status) return detail ?? null;
  return labelSize(label, "above", detail).width <= rect.width + LABEL_OVERHANG
    ? detail
    : { ...detail, status: "" };
}

function box(self, label, stage, insets, avoid, detail) {
  const placed = labelPlacement(
    self,
    null,
    stage,
    labelSize(label, "above", detail),
    insets,
    avoid,
  );
  return { ...placed, ...labelSize(label, placed.placement, detail) };
}

// One computer's displays inside a group node placed at the computer's bounding box; each display sits at
// its own tile rect, so the same node draws a grouped block and a free spread.
// `tone` names the color slot the group draws in ("local", "peer", "peer-2" ... "peer-6" for a third
// computer and beyond); it defaults to `side` so the two-computer callers that do not pass it yet
// still get the right tone for free. `state` is "offline" for a computer that cannot be reached.
export function createGroupNode({
  tiles,
  tileRects,
  groupKey,
  platform,
  side,
  tone,
  label,
  rect,
  labelBox,
  paint,
  state = null,
  quiet = false,
  focusable = false,
  focusTiles = false,
  ariaLabel,
  tileAria,
}) {
  const node = svgNode("g");
  node.classList.add("arrangement-group", `is-${platform}`);
  if (side) node.classList.add(`is-${side}`);
  node.dataset.group = groupKey;
  node.dataset.tone = tone ?? side ?? groupKey;
  if (state) node.dataset.state = state;
  if (quiet) node.dataset.quiet = "true";
  node.setAttribute("role", focusable ? "button" : "group");
  node.setAttribute("aria-label", ariaLabel ?? label);
  if (focusable) {
    node.dataset.focusKey = `arrangement-group-${groupKey}`;
    node.setAttribute("tabindex", "0");
  }
  setPosition(node, rect.x, rect.y);
  const labelNode = createGroupLabelNode(label, labelBox, rect);
  labelNode.dataset.motionId = `label:${groupKey}`;
  node.append(labelNode);
  for (const tile of tiles) {
    const local = tileRects[tile.id];
    node.append(
      displayNode(
        tile,
        { x: local.x - rect.x, y: local.y - rect.y, width: local.width, height: local.height },
        paint,
        focusTiles,
        tileAria?.(tile),
      ),
    );
  }
  return node;
}

export function createOutlineNode(rects, className) {
  const node = svgNode("g");
  node.classList.add(className);
  node.setAttribute("aria-hidden", "true");
  for (const rect of rects) {
    const outline = svgNode("rect");
    setRect(outline, rect);
    outline.setAttribute("rx", String(TILE_RADIUS));
    node.append(outline);
  }
  return node;
}

// Each crossing is a lit edge: the desk shows through a gap under it, a soft glow around it, and a
// handle at its middle where it is long enough to carry one. `offTiles` names displays whose
// computer cannot be reached; a seam touching one of them goes grey.
export function createSeamLayer(
  seams,
  transform,
  {
    className = "arrangement-seams",
    preview = false,
    offTiles = null,
    handles = !preview,
    keepClear = [],
  } = {},
) {
  const layer = svgNode("g");
  layer.classList.add(className);
  if (preview) layer.classList.add("is-preview");
  layer.setAttribute("aria-hidden", "true");
  const knobs = [];
  for (const seam of Array.isArray(seams) ? seams : []) {
    if (!validPoint(seam?.start) || !validPoint(seam?.end)) continue;
    const points = [
      transform.originX + seam.start[0] * transform.scale,
      transform.originY + seam.start[1] * transform.scale,
      transform.originX + seam.end[0] * transform.scale,
      transform.originY + seam.end[1] * transform.scale,
    ];
    const run = svgNode("g");
    run.classList.add("arrangement-seam-run");
    const ends = { from: String(seam.fromDisplay ?? ""), to: String(seam.toDisplay ?? "") };
    Object.assign(run.dataset, ends);
    const off = Boolean(offTiles?.has(ends.from) || offTiles?.has(ends.to));
    if (off) run.dataset.tone = "off";
    run.append(
      seamLine("arrangement-seam-underlay", points),
      seamLine("arrangement-seam-glow", points),
      seamLine("arrangement-seam", points),
    );
    layer.append(run);
    const length = Math.hypot(points[2] - points[0], points[3] - points[1]);
    const knob = handles && length >= HANDLE_MIN_SEAM ? seamHandle(points, off, keepClear) : null;
    if (knob) {
      Object.assign(knob.dataset, ends);
      knobs.push(knob);
    }
  }
  layer.append(...knobs);
  return layer;
}

function seamHandle([x1, y1, x2, y2], off, keepClear) {
  const handle = svgNode("g");
  handle.classList.add("arrangement-seam-handle");
  if (off) handle.dataset.tone = "off";
  const length = Math.hypot(x2 - x1, y2 - y1);
  const along = handleOffset(length, (t) => {
    const cx = x1 + ((x2 - x1) * t) / length;
    const cy = y1 + ((y2 - y1) * t) / length;
    return !keepClear.some((zone) => overlapsDisc(zone, cx, cy));
  });
  if (along === null) return null;
  setPosition(handle, x1 + ((x2 - x1) * along) / length, y1 + ((y2 - y1) * along) / length);
  const disc = svgNode("circle");
  disc.setAttribute("r", String(HANDLE_RADIUS));
  const glyph = svgNode("path");
  glyph.classList.add("arrangement-seam-glyph");
  glyph.setAttribute("d", SWAP_GLYPH);
  // A seam across a vertical edge is crossed sideways; one across a horizontal edge, up and down.
  const sideways = Math.abs(x2 - x1) < Math.abs(y2 - y1);
  glyph.setAttribute(
    "transform",
    `${sideways ? "" : "rotate(90) "}translate(-6 -6) scale(.5)`.trim(),
  );
  handle.append(disc, glyph);
  return handle;
}

export function createGuideLayer(guides, stage) {
  const layer = svgNode("g");
  layer.classList.add("arrangement-guides");
  layer.setAttribute("aria-hidden", "true");
  for (const guide of guides) {
    const line = svgNode("line");
    line.classList.add("arrangement-guide");
    const vertical = guide.axis === "x";
    line.setAttribute("x1", String(round(vertical ? guide.at : 0)));
    line.setAttribute("y1", String(round(vertical ? 0 : guide.at)));
    line.setAttribute("x2", String(round(vertical ? guide.at : stage.width)));
    line.setAttribute("y2", String(round(vertical ? stage.height : guide.at)));
    layer.append(line);
  }
  return layer;
}

// The arrangement's whole extent, measured under it: end ticks and a hairline that parts around
// the size. Too narrow for the line, the size stands alone.
export function createRulerNode({ x, y, width }, label) {
  const node = svgNode("g");
  node.classList.add("arrangement-ruler");
  node.setAttribute("aria-hidden", "true");
  setPosition(node, x, y);
  const textWidth = measureWith(RULER_FONT)(label) + RULER_TEXT_GAP * 2;
  if (width >= textWidth + RULER_TICK * 8) {
    const gapStart = (width - textWidth) / 2;
    node.append(
      rulerLine(0, RULER_TICK, gapStart, RULER_TICK),
      rulerLine(gapStart + textWidth, RULER_TICK, width, RULER_TICK),
      rulerLine(0, 0, 0, RULER_TICK * 2),
      rulerLine(width, 0, width, RULER_TICK * 2),
    );
  }
  const text = svgNode("text");
  text.classList.add("arrangement-ruler-text");
  text.setAttribute("x", String(round(width / 2)));
  text.setAttribute("y", String(RULER_TICK + 3.5));
  text.setAttribute("text-anchor", "middle");
  text.textContent = label;
  node.append(text);
  return node;
}

function rulerLine(x1, y1, x2, y2) {
  const line = svgNode("line");
  line.classList.add("arrangement-ruler-line");
  line.setAttribute("x1", String(round(x1)));
  line.setAttribute("y1", String(round(y1)));
  line.setAttribute("x2", String(round(x2)));
  line.setAttribute("y2", String(round(y2)));
  return line;
}

// Where a computer that is not on the canvas yet will go: a dashed slot with a plus and what it is.
export function createSlotNode(rect, { title, detail = "" }) {
  const node = svgNode("g");
  node.classList.add("arrangement-slot");
  node.setAttribute("aria-hidden", "true");
  setPosition(node, rect.x, rect.y);
  const frame = svgNode("rect");
  frame.classList.add("arrangement-slot-frame");
  setRect(frame, { x: 0, y: 0, width: rect.width, height: rect.height });
  frame.setAttribute("rx", String(SLOT_RADIUS));
  node.append(frame);
  const type = rect.width >= LARGE_TILE.width ? TILE_TYPE.large : TILE_TYPE.regular;
  const room = rect.width - TILE_PADDING * 2;
  const detailFont = `400 ${type.size}px ${FAMILY}`;
  const lines =
    detail && rect.height >= 64 && room >= 60 ? wrapLines(detail, detailFont, room) : [];
  const plus = rect.height >= 44;
  // The plus, the title and the detail lines stand as one block in the middle of the slot.
  const lineHeight = type.size + 3;
  const block = (plus ? SLOT_PLUS + SLOT_PLUS_GAP : 0) + type.name + lines.length * lineHeight;
  let y = (rect.height - block) / 2;
  if (plus) {
    node.append(plusGlyph(rect.width / 2, y + SLOT_PLUS / 2));
    y += SLOT_PLUS + SLOT_PLUS_GAP;
  }
  y += type.name * 0.8;
  node.append(
    textNode(
      "arrangement-slot-title",
      rect.width / 2,
      y,
      `600 ${type.name}px ${FAMILY}`,
      type.name,
      title,
      room,
      { anchor: "middle" },
    ),
  );
  y += type.name * 0.2 + 3;
  for (const line of lines) {
    y += lineHeight - 3;
    node.append(
      textNode("arrangement-slot-detail", rect.width / 2, y, detailFont, type.size, line, room, {
        anchor: "middle",
      }),
    );
    y += 3;
  }
  return node;
}

// At most two lines, broken between words; whatever still does not fit is cut on the second.
function wrapLines(text, font, room) {
  const measure = measureWith(font);
  const words = text.split(/\s+/).filter(Boolean);
  let first = "";
  while (words.length && measure(first ? `${first} ${words[0]}` : words[0]) <= room)
    first = first ? `${first} ${words.shift()}` : words.shift();
  if (!first) return [text];
  return words.length ? [first, words.join(" ")] : [first];
}

function plusGlyph(x, y) {
  const node = svgNode("path");
  node.classList.add("arrangement-slot-plus");
  node.setAttribute("d", `M${round(x - 5)} ${round(y)}h10M${round(x)} ${round(y - 5)}v10`);
  return node;
}

// The handle sits at the middle of its seam, or the nearest place along it clear of the displays'
// names and marks. A seam with no clear place goes without one; its glow still marks the crossing.
function handleOffset(length, clear) {
  const middle = length / 2;
  for (let step = 0; step <= middle - HANDLE_RADIUS; step += HANDLE_STEP)
    for (const along of [middle - step, middle + step]) if (clear(along)) return along;
  return null;
}

function overlapsDisc(zone, cx, cy) {
  const reach = HANDLE_RADIUS + HANDLE_CLEARANCE;
  return (
    cx + reach > zone.x &&
    cx - reach < zone.x + zone.width &&
    cy + reach > zone.y &&
    cy - reach < zone.y + zone.height
  );
}

// Where each display's name, size and marks are drawn, so seam handles can keep off them.
export function tileMarks(tiles, rects) {
  const zones = [];
  for (const display of tiles) {
    const rect = rects[display.id];
    if (!rect) continue;
    const { x, y, width, height } = rect;
    const metrics = TILE_TYPE[tileType(width, height)];
    if (hasMarks(width, height)) {
      if (display.primary)
        zones.push({
          x: x + width - TILE_PADDING - 6,
          y: y + TILE_PADDING - 2,
          width: 8,
          height: 8,
        });
      if (display.shared)
        zones.push({
          x: x + width - TILE_PADDING - 10,
          y: y + height - TILE_PADDING - 10,
          width: 10,
          height: 10,
        });
    }
    const textWidth = width - TILE_PADDING * 2;
    if (textWidth < 22 || height < 18) continue;
    const nameWidth = Math.min(
      textWidth,
      measureWith(`600 ${metrics.name}px ${FAMILY}`)(display.name),
    );
    if (height >= 40) {
      const sizeWidth = measureWith(`500 ${metrics.size}px ${MONO}`)(
        formatSize(display.width, display.height),
      );
      const block = metrics.name + metrics.gap + metrics.size;
      zones.push({
        x: x + TILE_PADDING,
        y: y + height - TILE_PADDING - 1 - block,
        width: Math.min(textWidth, Math.max(nameWidth, sizeWidth)),
        height: block,
      });
    } else
      zones.push({
        x: x + TILE_PADDING,
        y: y + height / 2 - metrics.name / 2,
        width: nameWidth,
        height: metrics.name,
      });
  }
  return zones;
}

function tileType(width, height) {
  return width >= LARGE_TILE.width && height >= LARGE_TILE.height ? "large" : "regular";
}

function hasMarks(width, height) {
  return width >= 30 && height >= 22;
}

// `tag` lets the canvas use the strip as its figure's caption.
export function createLegend(items, { tag = "div" } = {}) {
  const legend = document.createElement(tag);
  legend.className = "arrangement-legend";
  for (const item of items) {
    const entry = document.createElement("span");
    entry.className = "arrangement-legend-item";
    entry.dataset.kind = item.kind;
    if (item.platform) entry.dataset.platform = item.platform;
    if (item.side) entry.dataset.side = item.side;
    if (item.tone) entry.dataset.tone = item.tone;
    const text = document.createElement("span");
    text.textContent = item.label;
    if (item.kind !== "caption" && item.kind !== "hint") {
      const swatch = item.kind === "shared" ? icon("link", 10) : document.createElement("span");
      swatch.classList.add("arrangement-legend-swatch");
      swatch.setAttribute("aria-hidden", "true");
      entry.append(swatch);
    }
    entry.append(text);
    legend.append(entry);
  }
  return legend;
}

// Canvas metrics only estimate the CSS font; once the text is laid out its real advance width decides.
export function refineText(root) {
  for (const node of root.querySelectorAll?.("text[data-text]") ?? []) {
    if (typeof node.getComputedTextLength !== "function") return;
    const full = node.dataset.text ?? "";
    const maxWidth = Number(node.dataset.width);
    const estimate = node.textContent;
    node.textContent = full;
    const width = node.getComputedTextLength();
    if (!(width > 0) || !Number.isFinite(maxWidth)) {
      node.textContent = estimate;
      continue;
    }
    const shown =
      width <= maxWidth
        ? full
        : node.dataset.fit === "exact"
          ? ""
          : truncateToWidth(full, maxWidth, (value) => {
              node.textContent = value;
              return node.getComputedTextLength();
            });
    node.textContent = shown;
    node.setAttribute("visibility", shown ? "visible" : "hidden");
  }
}

// A display drawn as a small screen: its computer's tone glowing under a sheen, inside a thin
// bezel. The hatch and the Offline tag wait under it for the group to go offline.
function displayNode(display, { x, y, width, height }, paint, focusable, ariaLabel) {
  const tile = svgNode("g");
  tile.classList.add("arrangement-display");
  if (display.primary) tile.classList.add("is-primary");
  if (display.shared) tile.classList.add("is-shared");
  tile.dataset.display = display.id;
  tile.dataset.motionId = `tile:${display.id}`;
  const size = formatSize(display.width, display.height);
  tile.setAttribute("role", focusable ? "button" : "img");
  tile.setAttribute(
    "aria-label",
    ariaLabel ??
      `${display.name}, ${spokenSize(display.width, display.height)}${display.primary ? ", primary display" : ""}${display.shared ? ", cabled to both computers" : ""}`,
  );
  if (focusable) {
    tile.dataset.focusKey = `arrangement-display-${display.id}`;
    tile.setAttribute("tabindex", "0");
  }
  const tooltip = svgNode("title");
  tooltip.textContent = `${display.name} · ${size}${display.primary ? " · Primary display" : ""}${display.shared ? " · Cabled to both computers" : ""}`;
  const bezel = svgNode("rect");
  bezel.classList.add("arrangement-display-bezel");
  setRect(bezel, { x, y, width, height });
  bezel.setAttribute("rx", String(TILE_RADIUS));
  tile.append(tooltip, bezel);
  const screen = {
    x: x + BEZEL,
    y: y + BEZEL,
    width: width - BEZEL * 2,
    height: height - BEZEL * 2,
  };
  if (screen.width > 0 && screen.height > 0)
    for (const [className, fill] of [
      ["arrangement-display-screen", null],
      ["arrangement-display-depth", paint?.depth],
      ["arrangement-display-sheen", paint?.sheen],
      ["arrangement-display-hatch", paint?.hatch],
    ]) {
      if (fill === undefined) continue;
      const layer = svgNode("rect");
      layer.classList.add(className);
      setRect(layer, screen);
      layer.setAttribute("rx", String(SCREEN_RADIUS));
      // Paint ids are per drawing, so the fill is set here rather than in the stylesheet.
      if (fill) layer.style.setProperty("fill", fill);
      tile.append(layer);
    }

  const type = tileType(width, height);
  tile.dataset.type = type;
  const metrics = TILE_TYPE[type];
  const marked = display.primary && hasMarks(width, height);
  if (marked) {
    const marker = svgNode("circle");
    marker.classList.add("arrangement-display-primary");
    marker.setAttribute("cx", String(round(x + width - TILE_PADDING - 2)));
    marker.setAttribute("cy", String(round(y + TILE_PADDING + 2)));
    marker.setAttribute("r", "3");
    const markerTitle = svgNode("title");
    markerTitle.textContent = "Primary display";
    marker.append(markerTitle);
    tile.append(marker);
  }

  const shared = display.shared && hasMarks(width, height);
  if (shared) {
    const badge = icon("link", 10);
    badge.classList.add("arrangement-display-shared");
    badge.setAttribute("x", String(round(x + width - TILE_PADDING - 10)));
    badge.setAttribute("y", String(round(y + height - TILE_PADDING - 10)));
    const badgeTitle = svgNode("title");
    badgeTitle.textContent = "Cabled to both computers";
    badge.append(badgeTitle);
    tile.append(badge);
  }

  const textWidth = width - TILE_PADDING * 2;
  if (textWidth >= 40 && height >= 52)
    tile.append(
      textNode(
        "arrangement-display-offline",
        x + TILE_PADDING,
        y + TILE_PADDING + metrics.size - 1,
        `500 ${metrics.size}px ${MONO}`,
        metrics.size,
        "Offline",
        textWidth - (marked ? MARKER_ROOM : 0),
        { exact: true },
      ),
    );
  if (textWidth < 22 || height < 18) return tile;
  const nameFont = `600 ${metrics.name}px ${FAMILY}`;
  if (height >= 40) {
    // Name above the size, both resting on the bottom edge as on a real screen's corner.
    const sizeBaseline = y + height - TILE_PADDING - 1;
    const nameBaseline = sizeBaseline - metrics.size - metrics.gap;
    tile.append(
      textNode(
        "arrangement-display-name",
        x + TILE_PADDING,
        nameBaseline,
        nameFont,
        metrics.name,
        display.name,
        textWidth,
      ),
      // Half a resolution reads as a wrong number, so the size is shown whole or not at all.
      textNode(
        "arrangement-display-size",
        x + TILE_PADDING,
        sizeBaseline,
        `500 ${metrics.size}px ${MONO}`,
        metrics.size,
        size,
        textWidth - (shared ? MARKER_ROOM : 0),
        { exact: true },
      ),
    );
  } else {
    tile.append(
      textNode(
        "arrangement-display-name",
        x + TILE_PADDING,
        y + height / 2 + metrics.name * 0.35,
        nameFont,
        metrics.name,
        display.name,
        textWidth - (marked ? MARKER_ROOM : 0),
      ),
    );
  }
  return tile;
}

export function createGroupLabelNode(label, labelBox, rect) {
  const node = svgNode("g");
  node.classList.add("arrangement-group-label");
  node.dataset.placement = labelBox.placement;
  const detail = labelBox.detail ?? null;
  const x = labelBox.x - rect.x;
  const y = labelBox.y - rect.y;
  const inside = labelBox.placement === "inside";
  if (inside) {
    const plate = svgNode("rect");
    plate.classList.add("arrangement-group-label-plate");
    setRect(plate, { x, y, width: labelBox.width, height: labelBox.height });
    plate.setAttribute("rx", "4");
    node.append(plate);
  }
  let at = x + (inside ? LABEL_PLATE_PADDING : 0);
  if (detail?.tone) {
    const dot = svgNode("circle");
    dot.classList.add("arrangement-group-label-dot");
    dot.dataset.tone = detail.tone;
    dot.setAttribute("cx", String(round(at + LABEL_DOT / 2)));
    dot.setAttribute("cy", String(round(y + LABEL_HEIGHT / 2)));
    dot.setAttribute("r", String(LABEL_DOT / 2));
    node.append(dot);
    at += LABEL_DOT + LABEL_DOT_GAP;
  }
  const text = svgNode("text");
  text.classList.add("arrangement-group-label-text");
  text.setAttribute("x", String(round(at)));
  text.setAttribute("y", String(round(y + 11.5)));
  const name = svgNode("tspan");
  name.textContent = label;
  text.append(name);
  if (detail?.status) {
    const status = svgNode("tspan");
    status.classList.add("arrangement-group-label-status");
    status.setAttribute("dx", String(STATUS_GAP));
    status.textContent = detail.status;
    text.append(status);
  }
  node.append(text);
  return node;
}

function textNode(className, x, y, font, fontSize, full, maxWidth, options = {}) {
  const node = svgNode("text");
  node.classList.add(className);
  node.setAttribute("x", String(round(x)));
  node.setAttribute("y", String(round(y)));
  node.setAttribute("font-size", String(fontSize));
  if (options.anchor) node.setAttribute("text-anchor", options.anchor);
  node.dataset.text = full;
  node.dataset.width = String(Math.floor(maxWidth));
  if (options.exact) node.dataset.fit = "exact";
  const measure = measureWith(font);
  const shown =
    options.exact && measure(full) > maxWidth ? "" : truncateToWidth(full, maxWidth, measure);
  node.textContent = shown;
  if (!shown) node.setAttribute("visibility", "hidden");
  return node;
}

function seamLine(className, [x1, y1, x2, y2]) {
  const line = svgNode("line");
  line.classList.add(className);
  line.setAttribute("x1", String(round(x1)));
  line.setAttribute("y1", String(round(y1)));
  line.setAttribute("x2", String(round(x2)));
  line.setAttribute("y2", String(round(y2)));
  return line;
}

function setRect(node, { x, y, width, height }) {
  node.setAttribute("x", String(round(x)));
  node.setAttribute("y", String(round(y)));
  node.setAttribute("width", String(round(Math.max(0, width))));
  node.setAttribute("height", String(round(Math.max(0, height))));
}

let measureContext;

function measureWith(font) {
  const context = sharedContext();
  if (!context) return (value) => Array.from(String(value)).length * 6.2;
  return (value) => {
    context.font = font;
    return context.measureText(String(value)).width;
  };
}

function sharedContext() {
  if (measureContext === undefined) {
    try {
      measureContext = document.createElement("canvas").getContext("2d") ?? null;
    } catch {
      measureContext = null;
    }
  }
  return measureContext;
}

function validPoint(value) {
  return Array.isArray(value) && value.length === 2 && value.every(Number.isFinite);
}

function round(value) {
  return Math.round(value * 100) / 100;
}
