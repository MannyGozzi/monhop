import { formatSize } from "./arrangement-model.mjs";

// Every decision a computer card makes about its display strip and its Layouts list, kept apart
// from the DOM so the rules are checked without a browser. The card only draws what it returns.

// Focus is restored by key across a re-render, so two rows of one computer must never share one.
// The fingerprint's first bytes name the computer and a hash of the whole name names the row.
export function layoutRowKey(fingerprint, name) {
  return `${String(fingerprint ?? "").slice(0, 8)}-${hash(String(name ?? ""))}`;
}

// FNV-1a over the whole name: stable between renders, and short enough to keep the key well
// under the 80 characters a focus key may use.
function hash(value) {
  let sum = 0x811c9dc5;
  for (let index = 0; index < value.length; index += 1) {
    sum = Math.imul(sum ^ value.charCodeAt(index), 0x01000193) >>> 0;
  }
  return sum.toString(36);
}

// The library hands back its oldest entry first; every surface shows the newest first.
export function newestFirst(entries) {
  return Array.isArray(entries) ? entries.toReversed() : [];
}

// --- display strip ---------------------------------------------------------

// Each side falls back on its own: a link that is up but has not reported the other computer's
// displays yet still shows what it had last time, said plainly rather than left blank.
export function displayStripSides(setup) {
  return {
    local: sideDisplays(setup?.live?.localDisplays, setup?.localDisplays),
    peer: sideDisplays(setup?.live?.peerDisplays, setup?.peerDisplays),
  };
}

function sideDisplays(live, saved) {
  const current = Array.isArray(live) ? live : [];
  if (current.length) return { displays: current, lastSeen: false };
  const previous = Array.isArray(saved) ? saved : [];
  return { displays: previous, lastSeen: previous.length > 0 };
}

export function hasDisplayStrip(sides) {
  return sides.local.displays.length > 0 || sides.peer.displays.length > 0;
}

// Native pixels are what the monitor really shows; the logical size is only the fallback.
export function displayChipLabel(display) {
  const size = pair(display?.nativeSize) ?? pair(display?.size);
  const name = display?.name || "Display";
  return size ? `${name} · ${formatSize(size[0], size[1])}` : name;
}

function pair(value) {
  return Array.isArray(value) && value.length === 2 && value.every(Number.isFinite) ? value : null;
}

// --- layout history --------------------------------------------------------

const LOAD_BLOCKED = {
  inactive: "Use this computer to load its layouts.",
  disconnected: "Available once this computer is connected.",
  misfit: "This layout does not fit the displays connected now.",
};

// Load is drawn on every row and enabled only when pressing it would really load: the entry the
// row drew, on the computer in use, over a connected link. A blocked row says why.
export function loadGate({ isActive, connected, entry }) {
  if (!isActive) return { enabled: false, reason: LOAD_BLOCKED.inactive };
  if (!connected) return { enabled: false, reason: LOAD_BLOCKED.disconnected };
  if (entry?.fits !== true || !entry.layout) return { enabled: false, reason: LOAD_BLOCKED.misfit };
  return { enabled: true, reason: "" };
}

// Forget takes two presses. One row across the whole app is armed at a time, named by its
// computer and its exact layout, so the same name under another computer is never armed with it.
export function isForgetArmed(armed, fingerprint, name) {
  return armed?.fingerprint === fingerprint && armed?.name === name;
}

export function pressForget(armed, fingerprint, name) {
  return isForgetArmed(armed, fingerprint, name)
    ? { armed: null, forget: true }
    : { armed: { fingerprint, name }, forget: false };
}

// Reading a computer's list again replaces the entry the armed row named, so the confirm drops.
export function clearForgetFor(armed, fingerprint) {
  return armed?.fingerprint === fingerprint ? null : armed;
}

// A computer that is no longer paired has no rows left to confirm.
export function keepForgetArmed(armed, fingerprints) {
  return armed && [...fingerprints].includes(armed.fingerprint) ? armed : null;
}

// One pass over a computer's history: order, row keys, the armed confirm, and whether Load works.
// `pending` is that computer's own read or forget still in flight, which locks its rows alone.
export function layoutRows({ fingerprint, entries, armed, isActive, connected, busy, pending }) {
  const locked = busy === true || pending === true;
  return newestFirst(entries).map((entry) => ({
    entry,
    key: layoutRowKey(fingerprint, entry.name),
    armed: isForgetArmed(armed, fingerprint, entry.name),
    disabled: locked,
    load: loadGate({ isActive, connected, entry }),
  }));
}
