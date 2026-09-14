// One read-only drawing path for the arrangement editor and the dashboard preview.
import { formatSize, labelPlacement, spokenSize, truncateToWidth } from "./arrangement-model.mjs";
import { icon } from "./icons.mjs";

const SVG_NS = "http://www.w3.org/2000/svg";
const FAMILY =
  '-apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI Variable", "Segoe UI", ui-sans-serif, sans-serif';
const NAME_FONT = `500 11px ${FAMILY}`;
const SIZE_FONT = `500 10px ${FAMILY}`;
const LABEL_FONT = `600 11.5px ${FAMILY}`;
const LABEL_HEIGHT = 16;
const LABEL_PLATE_PADDING = 6;
const TILE_PADDING = 8;
const MARKER_ROOM = 13;

export function svgNode(name) {
  return document.createElementNS(SVG_NS, name);
}

export function setPosition(node, x, y) {
  node?.setAttribute("transform", `translate(${round(x)} ${round(y)})`);
}

function labelSize(label, placement = "above") {
  const width =
    measureWith(LABEL_FONT)(label) + (placement === "inside" ? LABEL_PLATE_PADDING * 2 : 0);
  return { width: Math.ceil(width), height: LABEL_HEIGHT };
}

// Place both labels together so the second one never lands on top of the first.
export function labelBoxes(rects, labels, stage, insets) {
  const source = box(rects.source, rects.destination, labels.source, stage, insets, []);
  return {
    source,
    destination: box(rects.destination, rects.source, labels.destination, stage, insets, [source]),
  };
}

function box(self, other, label, stage, insets, avoid) {
  const placed = labelPlacement(self, other, stage, labelSize(label), insets, avoid);
  return { ...placed, ...labelSize(label, placed.placement) };
}

// One computer's displays inside a group node placed at the computer's bounding box; each display sits at
// its own tile rect, so the same node draws a grouped block and a free spread.
export function createGroupNode({
  tiles,
  tileRects,
  groupKey,
  platform,
  side,
  label,
  rect,
  labelBox,
  focusable = false,
  focusTiles = false,
  ariaLabel,
  source = false,
  tileAria,
}) {
  const node = svgNode("g");
  node.classList.add("arrangement-group", `is-${platform}`);
  if (side) node.classList.add(`is-${side}`);
  if (source) node.classList.add("is-input");
  node.dataset.group = groupKey;
  node.setAttribute("role", focusable ? "button" : "group");
  node.setAttribute("aria-label", ariaLabel ?? label);
  if (focusable) {
    node.dataset.focusKey = `arrangement-group-${groupKey}`;
    node.setAttribute("tabindex", "0");
  }
  setPosition(node, rect.x, rect.y);
  node.append(createGroupLabelNode(label, labelBox, rect));
  for (const tile of tiles) {
    const local = tileRects[tile.id];
    node.append(
      monitorNode(
        tile,
        { x: local.x - rect.x, y: local.y - rect.y, width: local.width, height: local.height },
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
    outline.setAttribute("x", String(round(rect.x)));
    outline.setAttribute("y", String(round(rect.y)));
    outline.setAttribute("width", String(round(rect.width)));
    outline.setAttribute("height", String(round(rect.height)));
    outline.setAttribute("rx", "6");
    node.append(outline);
  }
  return node;
}

export function createSeamLayer(
  seams,
  transform,
  { className = "arrangement-seams", preview = false } = {},
) {
  const layer = svgNode("g");
  layer.classList.add(className);
  if (preview) layer.classList.add("is-preview");
  layer.setAttribute("aria-hidden", "true");
  for (const seam of Array.isArray(seams) ? seams : []) {
    if (!validPoint(seam?.start) || !validPoint(seam?.end)) continue;
    const points = [
      transform.originX + seam.start[0] * transform.scale,
      transform.originY + seam.start[1] * transform.scale,
      transform.originX + seam.end[0] * transform.scale,
      transform.originY + seam.end[1] * transform.scale,
    ];
    layer.append(
      seamLine("arrangement-seam-underlay", points),
      seamLine("arrangement-seam", points),
    );
  }
  return layer;
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

export function createLegend(items) {
  const legend = document.createElement("div");
  legend.className = "arrangement-legend";
  for (const item of items) {
    const entry = document.createElement("span");
    entry.className = "arrangement-legend-item";
    entry.dataset.kind = item.kind;
    if (item.platform) entry.dataset.platform = item.platform;
    if (item.side) entry.dataset.side = item.side;
    const swatch = item.kind === "shared" ? icon("link", 10) : document.createElement("span");
    swatch.classList.add("arrangement-legend-swatch");
    swatch.setAttribute("aria-hidden", "true");
    const text = document.createElement("span");
    text.textContent = item.label;
    entry.append(swatch, text);
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

function monitorNode(display, { x, y, width, height }, focusable, ariaLabel) {
  const monitor = svgNode("g");
  monitor.classList.add("arrangement-monitor");
  if (display.primary) monitor.classList.add("is-primary");
  if (display.shared) monitor.classList.add("is-shared");
  monitor.dataset.display = display.id;
  const size = formatSize(display.width, display.height);
  monitor.setAttribute("role", focusable ? "button" : "img");
  monitor.setAttribute(
    "aria-label",
    ariaLabel ??
      `${display.name}, ${spokenSize(display.width, display.height)}${display.primary ? ", primary display" : ""}${display.shared ? ", cabled to both computers" : ""}`,
  );
  if (focusable) {
    monitor.dataset.focusKey = `arrangement-display-${display.id}`;
    monitor.setAttribute("tabindex", "0");
  }
  const tooltip = svgNode("title");
  tooltip.textContent = `${display.name} · ${size}${display.primary ? " · Primary display" : ""}${display.shared ? " · Cabled to both computers" : ""}`;
  const rectangle = svgNode("rect");
  rectangle.setAttribute("x", String(round(x)));
  rectangle.setAttribute("y", String(round(y)));
  rectangle.setAttribute("width", String(round(width)));
  rectangle.setAttribute("height", String(round(height)));
  rectangle.setAttribute("rx", "6");
  monitor.append(tooltip, rectangle);

  const marked = display.primary && width >= 30 && height >= 22;
  if (marked) {
    const marker = svgNode("circle");
    marker.classList.add("arrangement-primary-marker");
    marker.setAttribute("cx", String(round(x + width - 9)));
    marker.setAttribute("cy", String(round(y + 9)));
    marker.setAttribute("r", "3.5");
    const markerTitle = svgNode("title");
    markerTitle.textContent = "Primary display";
    marker.append(markerTitle);
    monitor.append(marker);
  }

  const shared = display.shared && width >= 30 && height >= 22;
  if (shared) {
    const badge = icon("link", 10);
    badge.classList.add("arrangement-shared-marker");
    badge.setAttribute("x", String(round(x + width - 15)));
    badge.setAttribute("y", String(round(y + height - 15)));
    const badgeTitle = svgNode("title");
    badgeTitle.textContent = "Cabled to both computers";
    badge.append(badgeTitle);
    monitor.append(badge);
  }

  const textWidth = width - TILE_PADDING * 2 - (marked || shared ? MARKER_ROOM : 0);
  if (textWidth < 22 || height < 18) return monitor;
  if (height >= 40) {
    monitor.append(
      textNode(
        "arrangement-monitor-name",
        x + TILE_PADDING,
        y + height / 2 - 2,
        NAME_FONT,
        display.name,
        textWidth,
      ),
    );
    // Half a resolution reads as a wrong number, so the size is shown whole or not at all.
    monitor.append(
      textNode(
        "arrangement-monitor-size",
        x + TILE_PADDING,
        y + height / 2 + 11,
        SIZE_FONT,
        size,
        textWidth,
        true,
      ),
    );
  } else {
    monitor.append(
      textNode(
        "arrangement-monitor-name",
        x + TILE_PADDING,
        y + height / 2 + 4,
        NAME_FONT,
        display.name,
        textWidth,
      ),
    );
  }
  return monitor;
}

export function createGroupLabelNode(label, labelBox, rect) {
  const node = svgNode("g");
  node.classList.add("arrangement-group-label");
  node.dataset.placement = labelBox.placement;
  const x = labelBox.x - rect.x;
  const y = labelBox.y - rect.y;
  if (labelBox.placement === "inside") {
    const plate = svgNode("rect");
    plate.classList.add("arrangement-group-label-plate");
    plate.setAttribute("x", String(round(x)));
    plate.setAttribute("y", String(round(y)));
    plate.setAttribute("width", String(round(labelBox.width)));
    plate.setAttribute("height", String(round(labelBox.height)));
    plate.setAttribute("rx", "4");
    node.append(plate);
  }
  const text = svgNode("text");
  text.classList.add("arrangement-group-label-text");
  text.setAttribute(
    "x",
    String(round(x + (labelBox.placement === "inside" ? LABEL_PLATE_PADDING : 0))),
  );
  text.setAttribute("y", String(round(y + 11.5)));
  text.textContent = label;
  node.append(text);
  return node;
}

function textNode(className, x, y, font, full, maxWidth, exact = false) {
  const node = svgNode("text");
  node.classList.add(className);
  node.setAttribute("x", String(round(x)));
  node.setAttribute("y", String(round(y)));
  node.dataset.text = full;
  node.dataset.width = String(Math.floor(maxWidth));
  if (exact) node.dataset.fit = "exact";
  const measure = measureWith(font);
  const shown = exact && measure(full) > maxWidth ? "" : truncateToWidth(full, maxWidth, measure);
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
