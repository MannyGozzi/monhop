// The sidebar: This Mac, every paired computer, the add row and Settings. Rows outlive renders, so
// hover, focus and a label mid-fade carry on; the selection indicator glides from row to row.
import { presence } from "./accordion.mjs";
import {
  el,
  glideIndicator,
  motionEase,
  motionEnabled,
  motionMs,
  motionToken,
  pairBadge,
  platformGlyph,
  setLabel,
} from "./dom.mjs";
import { listStep } from "./studio-model.mjs";

// Each row's kept parts by its model key. A row's presence key is its own `slot`, not its model key,
// so a row that changes key (the pairing row becoming the computer it added) keeps its node.
const rows = new Map();
// The rows last placed, and everything drawn with them: rows on their way out stay where they left.
let placed = [];
let drawn = [];
let slots = 0;
let listLoaded = false;
let mounted = false;

export function renderSidebar(nodes, ctx) {
  const { studio, computers, actions } = ctx;
  if (!mounted) mountKeyboard(nodes);
  mounted = true;
  const live = studio.rows.map((model) => {
    let row = rows.get(model.key);
    if (!row && model.adopt && rows.has(model.adopt)) {
      row = rows.get(model.adopt);
      rows.delete(model.adopt);
      rows.set(model.key, row);
    }
    if (!row) {
      row = buildRow(actions);
      rows.set(model.key, row);
      // A row that joins a list already on screen grows in; the first list lands as it is.
      if (listLoaded) presence(row.slot, null);
    }
    updateRow(row, model);
    return row;
  });
  const keys = new Set(studio.rows.map((model) => model.key));
  for (const key of rows.keys()) if (!keys.has(key)) rows.delete(key);
  if (live.length !== placed.length || live.some((row, index) => row !== placed[index]))
    placeRows(nodes.sidebarList, live);
  nodes.sidebarCount.textContent = computers.items.length ? String(computers.items.length) : "";
  const settingsSelected = studio.selectionKey === "settings";
  nodes.sidebarSettings.setAttribute("aria-current", String(settingsSelected));
  const selected = settingsSelected
    ? nodes.sidebarSettings
    : (live.find((row) => row.node.getAttribute("aria-current") === "true")?.node ?? null);
  glideIndicator(nodes.sidebarIndicator, selected);
  listLoaded = listLoaded || computers.loaded;
}

function buildRow(actions) {
  const tile = el("span", { className: "sidebar-tile", attrs: { "aria-hidden": "true" } });
  const title = el("span", { className: "sidebar-row-title" });
  const dot = el("span", { className: "status-dot", attrs: { "aria-hidden": "true" } });
  const label = el("span", { className: "sidebar-row-label-text" });
  const picture = el("span", { className: "sidebar-picture", attrs: { "aria-hidden": "true" } });
  const node = el("button", {
    className: "sidebar-row",
    attrs: { type: "button" },
    children: [
      tile,
      el("span", {
        className: "sidebar-row-copy",
        children: [
          title,
          el("span", { className: "sidebar-row-label", children: [dot, label, picture] }),
        ],
      }),
    ],
  });
  node.addEventListener("click", () => actions.select(node.dataset.select));
  return {
    slot: `sidebar-row-${++slots}`,
    node,
    tile,
    title,
    dot,
    label,
    picture,
    glyph: undefined,
    badge: undefined,
  };
}

function updateRow(row, model) {
  const { node } = row;
  // A row's first labels land as they are; later changes fade in.
  const label = row.glyph === undefined ? writeText : setLabel;
  node.dataset.select = model.select;
  node.dataset.focusKey = `sidebar-${model.select}`;
  node.dataset.kind = model.kind;
  node.setAttribute("aria-current", String(model.selected));
  label(row.title, model.title);
  label(row.label, model.label);
  row.dot.dataset.tone = model.tone;
  row.picture.dataset.shown = String(model.badgeShown === true);
  const badgeKey = model.badge ? JSON.stringify(model.badge) : null;
  if (row.badge !== badgeKey) {
    row.badge = badgeKey;
    row.picture.replaceChildren(model.badge ? pairBadge(model.badge) : "");
  }
  const glyph = model.platform ? `platform:${model.platform}` : model.kind;
  if (row.glyph === glyph) return;
  const first = row.glyph === undefined;
  row.glyph = glyph;
  // Tinted by role as on the canvas: This Mac the local tone, every other computer the peer one.
  row.tile.dataset.kind = model.kind;
  row.tile.dataset.platform = model.platform ?? "";
  row.tile.replaceChildren(
    model.platform ? platformGlyph(model.platform) : el("span", { className: "add-glyph" }),
  );
  // A tile that becomes another computer's (the add row turning into the one it paired) pops in.
  if (!first && motionEnabled())
    row.tile.animate(
      [
        { opacity: 0, transform: `scale(${motionToken("--scale-pop")})` },
        { opacity: 1, transform: "none" },
      ],
      { duration: motionMs("--motion-spring"), easing: motionEase("--ease-spring") },
    );
}

function writeText(node, value) {
  node.textContent = value;
}

// Only a change to which rows are listed redraws the list. A row that left closes where it was,
// drawn as a ghost, while the rest glide up past it.
function placeRows(list, live) {
  const kept = new Set(live);
  const order = [...live];
  drawn.forEach((row, index) => {
    if (!kept.has(row)) order.splice(Math.min(index, order.length), 0, row);
  });
  const panels = [];
  const next = [];
  for (const row of order) {
    const panel = presence(row.slot, kept.has(row) ? row.node : null);
    if (!panel) continue;
    panels.push(panel);
    next.push(row);
  }
  list.replaceChildren(...panels);
  drawn = next;
  placed = live;
}

// Up and Down (and Home and End) walk the rows, Settings last; Enter and Space select, as buttons do.
function mountKeyboard(nodes) {
  nodes.sidebarList.parentElement.addEventListener("keydown", (event) => {
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const current = event.target.closest?.(".sidebar-row");
    if (!current) return;
    const order = [
      ...nodes.sidebarList.querySelectorAll(".sidebar-row:not([data-exit])"),
      nodes.sidebarSettings,
    ].filter((node) => !node.closest("[inert]"));
    const index = listStep(order.length, order.indexOf(current), event.key);
    if (index < 0) return;
    event.preventDefault();
    order[index].focus();
  });
}
