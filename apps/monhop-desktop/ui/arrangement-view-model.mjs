// Text, options-shaping and interaction math for the arrangement editor (arrangement-view.mjs), kept
// apart from the DOM so it runs under plain node:test. Geometry itself stays in arrangement-model.mjs;
// this file only turns that geometry into words, keyboard deltas and the options shape the view reads.
import { formatSize, movePlacement, resolvePlacement } from "./arrangement-model.mjs";

export const NUDGE_PIXELS = 12;
export const COARSE_NUDGE_PIXELS = 48;
const NUDGE_DIRECTIONS = {
  ArrowLeft: [-1, 0],
  ArrowRight: [1, 0],
  ArrowUp: [0, -1],
  ArrowDown: [0, 1],
};
const LEGACY_ORDER = ["local", "peer"];

// --- options ---------------------------------------------------------------

// `options.members` (new: `[{key, label, platform, tone}]`) or `options.local/peerPlatform` +
// `options.local/peerLabel` (legacy, still accepted) become one ordered member list: `{order,
// byKey}`, where `byKey[key]` is `{label, platform, tone}`. The legacy shape always produces
// "local"/"peer" keys with today's default labels and tones, so a caller that has not moved to
// `members` yet renders byte-identically.
export function membersFromOptions(options) {
  if (Array.isArray(options?.members) && options.members.length) {
    const order = [];
    const byKey = {};
    for (const member of options.members) {
      const key = typeof member?.key === "string" && member.key ? member.key : null;
      if (!key || Object.hasOwn(byKey, key)) return null;
      const index = order.length;
      order.push(key);
      byKey[key] = {
        label:
          typeof member.label === "string" && member.label ? member.label : `Computer ${index + 1}`,
        platform: member.platform ?? null,
        tone: typeof member.tone === "string" && member.tone ? member.tone : defaultTone(index),
      };
    }
    return { order, byKey };
  }
  return {
    order: [...LEGACY_ORDER],
    byKey: {
      local: {
        label: typeof options?.localLabel === "string" ? options.localLabel : "This computer",
        platform: options?.localPlatform ?? null,
        tone: "local",
      },
      peer: {
        label: typeof options?.peerLabel === "string" ? options.peerLabel : "The other computer",
        platform: options?.peerPlatform ?? null,
        tone: "peer",
      },
    },
  };
}

// The color slot a group draws in when the caller names none: the first member is "local", the
// second "peer" (today's two tones), a third through eighth are "peer-2".."peer-6" (groups cap at 8
// members; a ninth would reuse "peer-6" rather than draw in no color at all).
function defaultTone(index) {
  if (index === 0) return "local";
  if (index === 1) return "peer";
  return `peer-${Math.min(index, 6)}`;
}

// `value` keyed by member (the shape `displayUseChoices`, sharing-model.mjs, returns): every member
// key in `memberKeys` gets its list, or an empty one when the caller reported nothing for it.
export function normalizeUseChoices(memberKeys, value) {
  const result = {};
  for (const key of Array.isArray(memberKeys) ? memberKeys : []) {
    result[key] = Array.isArray(value?.[key]) ? value[key] : [];
  }
  return result;
}

// --- aria text ---------------------------------------------------------------

// The other group whose rect sits closest to `key`'s, by the gap between their boxes (zero when
// they already overlap on that axis). For exactly two groups this is always the only other one.
export function nearestGroup(key, rects) {
  const self = rects?.[key];
  if (!self) return null;
  let best = null;
  let bestGap = Infinity;
  for (const [candidate, rect] of Object.entries(rects ?? {})) {
    if (candidate === key || !rect) continue;
    const gap = rectGap(self, rect);
    if (gap < bestGap) {
      best = candidate;
      bestGap = gap;
    }
  }
  return best;
}

function rectGap(a, b) {
  const dx = Math.max(a.x - (b.x + b.width), b.x - (a.x + a.width), 0);
  const dy = Math.max(a.y - (b.y + b.height), b.y - (a.y + a.height), 0);
  return Math.hypot(dx, dy);
}

// Where `self` sits relative to `other`, in the same words the old two-computer "sideWord" used.
function directionWord(self, other) {
  if (self.x >= other.x + other.width) return "to the right";
  if (self.x + self.width <= other.x) return "to the left";
  if (self.y >= other.y + other.height) return "below";
  if (self.y + self.height <= other.y) return "above";
  return "beside";
}

// The N-computer generalization of "sideWord": `key`'s position relative to whichever other group
// is nearest it. Two groups always compare against the same, only, other one and so read exactly as
// before; three or more compare against the closest neighbor, so a block deep in a chain still gets
// a sensible direction instead of being measured against every other block at once.
export function neighborWord(key, rects) {
  const other = nearestGroup(key, rects);
  return other ? directionWord(rects[key], rects[other]) : "";
}

// The group's own aria-label: name, display count and size, where it sits, whether it is touching
// yet, and how to move it. `crossingText` is `describeArrangement(arrangement)` when connected.
export function groupAriaText({ key, tiles, groups, rects, labels, connected, crossingText }) {
  const own = tiles.filter((t) => (t.group ?? t.side) === key);
  const count = own.length;
  const other = nearestGroup(key, rects);
  const place = other ? `Sits ${directionWord(rects[key], rects[other])} of ${labels[other]}.` : "";
  const crossing = connected ? crossingText : "Not touching yet.";
  const size = formatSize(groups[key].width, groups[key].height);
  return `${labels[key]}. ${count} display${count === 1 ? "" : "s"}, ${size} together. ${place} ${crossing} Drag, or use the arrow keys.`;
}

// Every seam that touches this tile, whichever side of the seam it is on: a middle block in a chain
// of three or more can be the "from" end of one crossing and the "to" end of another.
function tileSeams(tileId, seams) {
  return (seams ?? []).filter((seam) => seam.fromDisplay === tileId || seam.toDisplay === tileId);
}

function tileEdgeIn(tile, seam) {
  return seam.fromDisplay === tile.id ? seam.fromEdge : seam.toEdge;
}

// The tile's own aria-label. `otherGroupCount` picks the wording for "nothing touches yet": with
// exactly one other computer it names it ("the other computer", today's wording); with several, no
// single one is implied ("another computer").
export function tileAriaText({ tile, groupLabel, seams, otherGroupCount }) {
  const touching = tileSeams(tile.id, seams);
  const contact = touching.length
    ? `Crosses on its ${touching.map((seam) => tileEdgeIn(tile, seam)).join(" and ")} edge.`
    : otherGroupCount === 1
      ? "Not touching the other computer."
      : "Not touching another computer.";
  return `${tile.name}, ${formatSize(tile.width, tile.height)}${tile.primary ? ", primary display" : ""}, on ${groupLabel}. ${contact} Drag, or use the arrow keys.`;
}

// --- keyboard nudge ----------------------------------------------------------

export function nudgeDirection(key) {
  return NUDGE_DIRECTIONS[key] ?? null;
}

// The screen-pixel offset one arrow-key press moves a block by; null for a key that is not an arrow.
export function nudgePixels(key, shiftKey) {
  const direction = nudgeDirection(key);
  if (!direction) return null;
  const step = shiftKey ? COARSE_NUDGE_PIXELS : NUDGE_PIXELS;
  return [direction[0] * step, direction[1] * step];
}

// Where an arrow-key nudge would land: the resolved placement (clamped to the nearest legal
// touching position, per `resolvePlacement`, arrangement-model.mjs) or null when that direction has
// no legal position from here. Pure wrapper so the view only has to apply or reject the result.
export function nudgeTarget(groups, placement, moving, key, shiftKey, scale) {
  const pixels = nudgePixels(key, shiftKey);
  if (!pixels || !Number.isFinite(scale) || scale <= 0) return null;
  const moved = movePlacement(groups, placement, moving, [pixels[0] / scale, pixels[1] / scale]);
  return resolvePlacement(groups, moved, moving);
}
