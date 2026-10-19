// Pure logic behind the floating menus over the arranging canvas: where a menu sits beside what
// opened it, a saved arrangement drawn small, and the rows of the displays menu.
import { formatSize } from "./arrangement-model.mjs";

// Air between a menu and what it points at, and between a menu and the pane's edges.
export const MENU_GAP = 8;
export const MENU_INSET = 12;

// `prefer`'s side of the anchor when the menu fits there, else the other side when it fits there,
// else the roomier one; centred on the anchor, or along its start edge, and kept inside `bounds`.
// `origin` is where the anchor's centre falls across the menu, the point it grows from.
export function menuPlacement({ anchor, size, bounds, prefer = "below", align = "center" }) {
  const room = {
    below: bounds.bottom - anchor.bottom - MENU_GAP - MENU_INSET,
    above: anchor.top - bounds.top - MENU_GAP - MENU_INSET,
  };
  const other = prefer === "below" ? "above" : "below";
  let side = room[prefer] >= room[other] ? prefer : other;
  if (room[prefer] >= size.height) side = prefer;
  else if (room[other] >= size.height) side = other;
  const centre = anchor.left + anchor.width / 2;
  const wanted = align === "start" ? anchor.left : centre - size.width / 2;
  const x = within(wanted, bounds.left + MENU_INSET, bounds.right - MENU_INSET - size.width);
  const y = within(
    side === "below" ? anchor.bottom + MENU_GAP : anchor.top - MENU_GAP - size.height,
    bounds.top + MENU_INSET,
    bounds.bottom - MENU_INSET - size.height,
  );
  return {
    x: Math.round(x),
    y: Math.round(y),
    side,
    origin: Math.round(within(centre - x, 0, size.width)),
  };
}

// Low wins when the range is empty, so an oversized menu keeps its start edge in view.
function within(value, low, high) {
  return Math.max(low, Math.min(value, high));
}

// Displays scaled into `box` and centred there, each drawn `inset` smaller on every side so
// neighbours read as separate screens.
export function thumbnailRects(tiles, box, inset = 0) {
  if (!Array.isArray(tiles) || !tiles.length) return [];
  const left = Math.min(...tiles.map((tile) => tile.x));
  const top = Math.min(...tiles.map((tile) => tile.y));
  const width = Math.max(...tiles.map((tile) => tile.x + tile.width)) - left;
  const height = Math.max(...tiles.map((tile) => tile.y + tile.height)) - top;
  if (!(width > 0 && height > 0)) return [];
  const scale = Math.min(box.width / width, box.height / height);
  const offsetX = (box.width - width * scale) / 2;
  const offsetY = (box.height - height * scale) / 2;
  return tiles.map((tile) => ({
    id: tile.id,
    group: tile.group,
    x: round(offsetX + (tile.x - left) * scale + inset),
    y: round(offsetY + (tile.y - top) * scale + inset),
    width: round(Math.max(tile.width * scale - inset * 2, 1)),
    height: round(Math.max(tile.height * scale - inset * 2, 1)),
  }));
}

function round(value) {
  return Math.round(value * 100) / 100;
}

// One section per computer with displays, in the group's order, each display with what it is and
// whether its switch can move. `scope` keeps one computer's section, for a menu opened from one of
// its displays; `selected` marks the display that opened it.
export function displaySections({ choices, members, scope = null, selected = null }) {
  return members.order
    .filter((key) => (scope === null || key === scope) && choices[key]?.length)
    .map((key) => ({
      key,
      label: members.byKey[key].label,
      tone: members.byKey[key].tone,
      rows: choices[key].map((display) => {
        const locked = display.inUse && !display.canLeave;
        const details = displayDetails(display, locked);
        return {
          id: display.id,
          name: display.name,
          details,
          detail: details.join(" · "),
          inUse: display.inUse,
          locked,
          selected: display.id === selected,
          aspect: display.size[0] / display.size[1],
        };
      }),
    }));
}

function displayDetails(display, locked) {
  const parts = [formatSize(display.size[0], display.size[1])];
  if (display.primary) parts.push("Primary");
  if (display.cabledToBoth) parts.push("Cabled to both");
  if (locked) parts.push("Its computer's only display");
  return parts;
}

// The computer a display belongs to, for a menu opened from that display.
export function displayOwner(choices, id) {
  return (
    Object.keys(choices).find((key) => choices[key].some((display) => display.id === id)) ?? null
  );
}
