import { monitorKey } from "./arrangement-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import { normalizeArrangements, normalizeStoredLayout } from "./sharing-model.mjs";

const FINGERPRINT = /^[a-f0-9]{64}$/i;
const PLATFORM = new Set(["windows", "macos"]);
const SIDE = new Set(["local", "peer"]);
const MAX_U64 = "18446744073709551615";
const MAX_COMPUTERS = 16;

export function initialComputers() {
  return { loaded: false, items: [], active: null, interfaceId: null };
}

// --- each computer's layout history, kept outside the polled `computers` reply ------------

const EMPTY_ARRANGEMENTS = Object.freeze({ items: [], loading: false, error: "" });

export function initialComputerArrangements() {
  return {};
}

export function computerArrangements(store, fingerprint) {
  return store[fingerprint] ?? EMPTY_ARRANGEMENTS;
}

export function beginComputerArrangements(store, fingerprint) {
  return {
    ...store,
    [fingerprint]: { ...computerArrangements(store, fingerprint), loading: true, error: "" },
  };
}

export function setComputerArrangements(store, fingerprint, value) {
  return {
    ...store,
    [fingerprint]: { items: normalizeArrangements(value), loading: false, error: "" },
  };
}

export function failComputerArrangements(store, fingerprint, error) {
  return {
    ...store,
    [fingerprint]: { ...computerArrangements(store, fingerprint), loading: false, error },
  };
}

// A computer that is no longer paired keeps no layout history around to go stale.
export function pruneComputerArrangements(store, computers) {
  const known = new Set(computers.items.map((item) => item.fingerprint));
  const next = {};
  for (const [fingerprint, entry] of Object.entries(store))
    if (known.has(fingerprint)) next[fingerprint] = entry;
  return next;
}

export function normalizeComputers(value) {
  if (
    !value ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    !Array.isArray(value.computers)
  )
    return initialComputers();
  const seen = new Set();
  const items = [];
  for (const entry of value.computers) {
    const computer = normalizeComputer(entry);
    if (!computer || seen.has(computer.fingerprint) || items.length >= MAX_COMPUTERS) continue;
    seen.add(computer.fingerprint);
    items.push(computer);
  }
  // An active fingerprint nobody is paired with is dropped rather than carried as a state nothing can render.
  const active = cleanFingerprint(value.active);
  return {
    loaded: true,
    items,
    active: active && seen.has(active) ? active : null,
    interfaceId: cleanText(value.interfaceId, 512) || null,
  };
}

export function findComputer(computers, fingerprint) {
  return computers.items.find((item) => item.fingerprint === fingerprint) ?? null;
}

export function displayName(computer) {
  return computer?.name || platformLabel(computer?.platform === "macos" ? "macos" : "windows");
}

function normalizeComputer(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const fingerprint = cleanFingerprint(value.fingerprint);
  const platform = PLATFORM.has(value.platform) ? value.platform : null;
  if (!fingerprint || !platform) return null;
  return {
    fingerprint,
    platform,
    name: cleanText(value.name, 48),
    address: cleanText(value.address, 128) || null,
    setup: normalizeSetup(value.setup),
  };
}

function normalizeSetup(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return emptySetup();
  return {
    saved: value.saved === true,
    sourceSide: SIDE.has(value.sourceSide) ? value.sourceSide : null,
    localDisplays: normalizeDisplays(value.localDisplays),
    peerDisplays: normalizeDisplays(value.peerDisplays),
    layout: normalizeStoredLayout(value.layout),
    previewLayout: normalizeStoredLayout(value.previewLayout),
    live: normalizeLive(value.live),
    message: cleanText(value.message, 240),
  };
}

function emptySetup() {
  return {
    saved: false,
    sourceSide: null,
    localDisplays: [],
    peerDisplays: [],
    layout: null,
    previewLayout: null,
    live: null,
    message: "",
  };
}

// Present only while this computer has a live link or session; the same display shape as `localDisplays`.
function normalizeLive(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  return {
    localDisplays: normalizeDisplays(value.localDisplays),
    peerDisplays: normalizeDisplays(value.peerDisplays),
  };
}

function normalizeDisplays(value) {
  if (!Array.isArray(value)) return [];
  return value.map(normalizeDisplay).filter(Boolean).slice(0, 16);
}

// `nativeSize` and `scale` describe what the monitor really shows behind the logical size; they
// are optional, and an unusable one is dropped rather than taking the whole display with it.
function normalizeDisplay(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const origin = coordinate(value.origin);
  const size = sizePair(value.size);
  const id = value.id === undefined ? null : displayId(value.id);
  if (!origin || !size || (value.id !== undefined && !id)) return null;
  return {
    id,
    name: cleanText(value.name, 80) || "Display",
    origin,
    size,
    nativeSize: sizePair(value.nativeSize),
    scale: Number.isFinite(value.scale) && value.scale > 0 ? value.scale : null,
    primary: value.primary === true,
    monitor: monitorKey(value.monitor),
  };
}

function coordinate(value) {
  if (
    !Array.isArray(value) ||
    value.length !== 2 ||
    !value.every((item) => Number.isFinite(item) && Math.abs(item) <= 100_000)
  )
    return null;
  return [value[0], value[1]];
}

function sizePair(value) {
  if (
    !Array.isArray(value) ||
    value.length !== 2 ||
    !value.every((item) => Number.isFinite(item) && item > 0 && item <= 100_000)
  )
    return null;
  return [value[0], value[1]];
}

function displayId(value) {
  return typeof value === "string" &&
    (value === "0" || /^[1-9][0-9]{0,19}$/.test(value)) &&
    (value.length < MAX_U64.length || (value.length === MAX_U64.length && value <= MAX_U64))
    ? value
    : null;
}

// Fingerprints are lowercased here so every comparison in the UI is a plain ===.
function cleanFingerprint(value) {
  return typeof value === "string" && FINGERPRINT.test(value) ? value.toLowerCase() : null;
}

function cleanText(value, maximum) {
  if (typeof value !== "string") return "";
  const text = value.trim();
  if (
    !text ||
    text.length > maximum ||
    // oxlint-disable-next-line no-control-regex -- strips control/bidi-override chars by design
    /[\u0000-\u001f\u007f-\u009f\u200e\u200f\u202a-\u202e\u2066-\u2069]/.test(text)
  )
    return "";
  return text;
}
