// The sidebar: This Mac, every paired computer, the add row and Settings. Rows outlive renders, so
// hover, focus and a label mid-fade carry on; the selection indicator glides from row to row.
import { presence } from "./accordion.mjs";
import { el, motionEase, motionEnabled, motionMs, platformGlyph, setLabel } from "./dom.mjs";

// Each row's kept node by key, the keys the list last drew in order (rows on their way out
// included, at the place they left), and whether a list has been read yet.
const rows = new Map();
let drawnKeys = [];
let liveKeys = [];
let listLoaded = false;
let indicatorAnimation = null;

export function renderSidebar(nodes, ctx) {
  const { studio, computers, actions } = ctx;
  const keys = studio.rows.map((model) => model.key);
  for (const model of studio.rows) {
    let row = rows.get(model.key);
    if (!row) {
      row = buildRow(model, actions);
      rows.set(model.key, row);
      // A row that joins a list already on screen grows in; the first list lands as it is.
      if (listLoaded) presence(rowKey(model.key), null);
    }
    updateRow(row, model);
  }
  if (keys.join("\n") !== liveKeys.join("\n")) placeRows(nodes.sidebarList, keys);
  nodes.sidebarCount.textContent = computers.items.length ? String(computers.items.length) : "";
  const settingsSelected = studio.selectionKey === "settings";
  nodes.sidebarSettings.setAttribute("aria-current", String(settingsSelected));
  const selected = settingsSelected
    ? nodes.sidebarSettings
    : (rows.get(studio.selectionKey)?.node ?? null);
  placeIndicator(nodes.sidebarIndicator, selected);
  listLoaded = listLoaded || computers.loaded;
}

function rowKey(key) {
  return `sidebar-row-${key}`;
}

function buildRow(model, actions) {
  const tile = el("span", { className: "sidebar-tile", attrs: { "aria-hidden": "true" } });
  const title = el("span", { className: "sidebar-row-title" });
  const dot = el("span", { className: "status-dot", attrs: { "aria-hidden": "true" } });
  const label = el("span", { className: "sidebar-row-label-text" });
  const node = el("button", {
    className: "sidebar-row",
    attrs: { type: "button" },
    dataset: { select: model.key, focusKey: `sidebar-${model.key}` },
    children: [
      tile,
      el("span", {
        className: "sidebar-row-copy",
        children: [title, el("span", { className: "sidebar-row-label", children: [dot, label] })],
      }),
    ],
  });
  node.addEventListener("click", () => actions.select(node.dataset.select));
  return { node, tile, title, dot, label, platform: undefined };
}

function updateRow(row, model) {
  row.node.dataset.kind = model.kind;
  row.node.setAttribute("aria-current", String(model.selected));
  row.title.textContent = model.title;
  setLabel(row.label, model.label);
  row.dot.dataset.tone = model.tone;
  if (row.platform === model.platform) return;
  row.platform = model.platform;
  row.tile.dataset.kind = model.kind;
  row.tile.replaceChildren(
    model.platform ? platformGlyph(model.platform) : el("span", { className: "add-glyph" }),
  );
}

// Only a change to which rows are listed redraws the list. A row that left closes where it was,
// drawn as a ghost, while the rest glide up past it.
function placeRows(list, keys) {
  const live = new Set(keys);
  const order = [...keys];
  drawnKeys.forEach((key, index) => {
    if (!live.has(key)) order.splice(Math.min(index, order.length), 0, key);
  });
  const panels = [];
  const drawn = [];
  for (const key of order) {
    const panel = presence(rowKey(key), live.has(key) ? rows.get(key).node : null);
    if (!live.has(key)) rows.delete(key);
    if (!panel) continue;
    panels.push(panel);
    drawn.push(key);
  }
  list.replaceChildren(...panels);
  drawnKeys = drawn;
  liveKeys = keys;
}

// The one indicator moves into the selected row and slides over from wherever it was drawn, so a
// change mid-slide carries on from there.
function placeIndicator(indicator, row) {
  if (!row) {
    indicator.hidden = true;
    return;
  }
  indicator.hidden = false;
  if (indicator.parentElement === row) return;
  const from = indicator.getClientRects().length ? indicator.getBoundingClientRect() : null;
  indicatorAnimation?.cancel();
  indicatorAnimation = null;
  row.prepend(indicator);
  if (!from || !motionEnabled()) return;
  const to = indicator.getBoundingClientRect();
  if (!to.width || !to.height) return;
  const transform = `translate(${from.left - to.left}px, ${from.top - to.top}px) scale(${from.width / to.width}, ${from.height / to.height})`;
  indicatorAnimation = indicator.animate([{ transform }, { transform: "none" }], {
    duration: motionMs("--motion-slow"),
    easing: motionEase("--ease-glide"),
  });
}
