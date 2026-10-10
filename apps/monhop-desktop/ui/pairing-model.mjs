import { normalizePairBadge } from "./pair-badge-model.mjs";

const PHASES = new Set([
  "closed",
  "identity-missing",
  "ready",
  "showing",
  "connecting",
  "verifying",
  "saving",
  "paired",
  "error",
  "stopping",
]);
// Phases with a code out or a handshake running; the backend reports busy in exactly these.
const LIVE_PHASES = new Set(["showing", "connecting", "verifying", "saving", "stopping"]);
// Phases from which a new code can be shown or entered.
const IDLE_PHASES = new Set(["ready", "error", "paired"]);
const ROLES = new Set(["showing", "entering"]);
const PLATFORMS = new Set(["macos", "windows"]);
const STORAGE_OUTCOMES = new Set(["unchanged", "unverified", "verified"]);
const FINGERPRINT = /^[0-9a-fA-F]{64}$/;

// Crockford base32 as the backend parses it: no I, L, O or U, which read as 1, 1, 0 and nothing.
const CODE_SYMBOLS = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_ALIASES = { O: "0", I: "1", L: "1" };
export const CODE_LENGTH = 12;
const CODE_GROUP = 4;
const CODE_FORMAT = /^[0-9A-HJKMNP-TV-Z]{4}-[0-9A-HJKMNP-TV-Z]{4}-[0-9A-HJKMNP-TV-Z]{4}$/;
const MAX_CODE_INPUT = 64;

// `mode` is the step the user picked while no attempt runs: "choose" (the two options) or "enter"
// (typing a code). `codeShownAt` is when this screen first saw the code it shows.
export function initialPairingState() {
  return {
    mode: null,
    entry: "",
    entryError: "",
    codeShownAt: null,
    message: "",
    view: null,
  };
}

export function applyPairingView(state, value, now = Date.now()) {
  const view = normalizePairingView(value);
  const sameCode = view.code !== null && view.code === state.view?.code;
  const attempt = LIVE_PHASES.has(view.phase) || view.phase === "paired";
  return {
    ...state,
    // A running or finished attempt replaces whatever the user had picked before it.
    mode: attempt ? null : state.mode,
    entry: view.phase === "paired" ? "" : state.entry,
    entryError: attempt ? "" : state.entryError,
    codeShownAt: view.code === null ? null : sameCode ? (state.codeShownAt ?? now) : now,
    message: "",
    view,
  };
}

// Switching steps starts the field over; a code that just failed is never offered again.
export function choosePairingMode(state, mode) {
  const next = mode === "enter" || mode === "choose" ? mode : null;
  return {
    ...state,
    mode: next,
    entry: next === "enter" && state.mode === "enter" ? state.entry : "",
    entryError: "",
    message: "",
  };
}

export function editPairingEntry(state, value) {
  return { ...state, entry: formatCodeInput(value).value, entryError: "" };
}

export function failPairingEntry(state, message) {
  return {
    ...state,
    entryError: boundedText(message, 1000) || "That code did not work. Check it and try again.",
  };
}

export function pairingFailure(state, message) {
  return { ...state, message: boundedText(message, 1000) || "Pairing did not finish. Try again." };
}

// Which screen the exchange shows. Expiry is judged locally too, so the code never sits on screen
// past its deadline while the next status poll is on its way.
export function pairingStep(state, now = Date.now()) {
  const view = state.view;
  if (!view || view.phase === "closed") return "closed";
  if (view.phase === "identity-missing") return "identity";
  if (IDLE_PHASES.has(view.phase) && state.mode === "enter") return "enter";
  if (IDLE_PHASES.has(view.phase) && (state.mode === "choose" || view.phase === "ready"))
    return "choose";
  if (view.phase === "showing") return codeTimeLeft(view, now) === 0 ? "expired" : "show";
  if (LIVE_PHASES.has(view.phase)) return "progress";
  if (view.phase === "paired") return "paired";
  return "error";
}

export function canStartPairing(state) {
  return IDLE_PHASES.has(state.view?.phase) && !state.view.busy;
}

export function canSubmitPairingCode(state) {
  return canStartPairing(state) && codeSymbols(state.entry).length === CODE_LENGTH;
}

export function isBusyPairing(state) {
  return state.view?.busy === true || LIVE_PHASES.has(state.view?.phase);
}

// A code on screen or a handshake under way; leaving the exchange cancels it.
export function isPairingAttemptLive(state) {
  return ["showing", "connecting", "verifying", "saving"].includes(state.view?.phase);
}

// --- the short code ---------------------------------------------------------------------------

export function codeSymbols(value) {
  let symbols = "";
  for (const raw of String(value ?? "")
    .slice(0, MAX_CODE_INPUT)
    .toUpperCase()) {
    const symbol = CODE_ALIASES[raw] ?? raw;
    if (CODE_SYMBOLS.includes(symbol)) symbols += symbol;
    if (symbols.length === CODE_LENGTH) break;
  }
  return symbols;
}

function groupSymbols(symbols) {
  return (symbols.match(new RegExp(`.{1,${CODE_GROUP}}`, "g")) ?? []).join("-");
}

// Typing or pasting reformats the field: case, spaces and dashes never matter, dashes come back
// between groups, and the caret stays after the same symbol it followed.
export function formatCodeInput(raw, caret = String(raw ?? "").length) {
  const text = String(raw ?? "");
  const symbols = codeSymbols(text);
  const before = Math.min(codeSymbols(text.slice(0, caret)).length, symbols.length);
  return {
    value: groupSymbols(symbols),
    caret: before + Math.floor(Math.max(before - 1, 0) / CODE_GROUP),
    symbols,
  };
}

// The three groups a shown code is drawn in, and the label that reads it one symbol at a time.
export function codeGroups(code) {
  return CODE_FORMAT.test(code ?? "") ? code.split("-") : [];
}

export function spokenCode(code) {
  return codeGroups(code)
    .map((group) => group.split("").join(" "))
    .join(", ");
}

export function codeTimeLeft(view, now = Date.now()) {
  if (!Number.isFinite(view?.codeExpiresAtMs)) return null;
  return Math.max(0, view.codeExpiresAtMs - now);
}

export function formatCountdown(ms) {
  const seconds = Math.ceil(Math.max(0, ms) / 1000);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

// How far the countdown bar has drained: the whole window counts from when this screen first saw
// the code, since the view only carries its deadline.
export function countdownProgress(state, now = Date.now()) {
  const left = codeTimeLeft(state.view, now);
  const shownAt = state.codeShownAt;
  if (left === null || !Number.isFinite(shownAt)) return null;
  const total = Math.max(state.view.codeExpiresAtMs - shownAt, 1);
  return { total, elapsed: Math.min(Math.max(now - shownAt, 0), total), left: left / total };
}

// --- errors -----------------------------------------------------------------------------------

// The backend's sentence names what went wrong; the role decides the way forward.
export function pairingErrorPresentation(view) {
  const retry =
    view?.role === "showing" ? "new-code" : view?.role === "entering" ? "enter-again" : "reopen";
  const fallback = {
    "new-code": "This code no longer works. Show a new code.",
    "enter-again": "That code didn't work. Ask for a new code on the other computer.",
    reopen: "The pairing did not finish.",
  }[retry];
  return { title: "Pairing did not finish", detail: view?.message || fallback, retry };
}

// --- the native view --------------------------------------------------------------------------

function hasFullFingerprint(value) {
  return typeof value === "string" && FINGERPRINT.test(value);
}

export function normalizePairingView(value) {
  const source = value && typeof value === "object" ? value : {};
  const incompleteTrust =
    source.phase === "paired" &&
    (source.storageOutcome !== "verified" ||
      !hasFullFingerprint(source.localFingerprint) ||
      !hasFullFingerprint(source.peerFingerprint));
  const phase = !incompleteTrust && PHASES.has(source.phase) ? source.phase : "error";
  const message = incompleteTrust
    ? "The saved pairing is incomplete. Reload pairing before continuing."
    : boundedText(source.message, 1000);
  const code =
    phase === "showing" && typeof source.code === "string" && CODE_FORMAT.test(source.code)
      ? source.code
      : null;
  return {
    phase,
    role: ROLES.has(source.role) ? source.role : null,
    code,
    codeExpiresAtMs:
      code !== null && Number.isFinite(source.codeExpiresAtMs) && source.codeExpiresAtMs > 0
        ? source.codeExpiresAtMs
        : null,
    message:
      message || (phase === "error" ? "The pairing state was not recognized. Reload pairing." : ""),
    busy: source.busy === true,
    localFingerprint: fingerprintText(source.localFingerprint),
    peerFingerprint: fingerprintText(source.peerFingerprint),
    peerAddress: nullableText(source.peerAddress, 256),
    localPlatform: PLATFORMS.has(source.localPlatform) ? source.localPlatform : null,
    peerPlatform: PLATFORMS.has(source.peerPlatform) ? source.peerPlatform : null,
    badge: phase === "paired" ? normalizePairBadge(source.badge) : null,
    storageOutcome: STORAGE_OUTCOMES.has(source.storageOutcome)
      ? source.storageOutcome
      : "unverified",
  };
}

export function platformLabel(platform, local = false, short = false) {
  const name =
    platform === "macos"
      ? "Mac"
      : platform === "windows"
        ? short
          ? "Windows"
          : "Windows PC"
        : "computer";
  return local ? `This ${name}` : name;
}

function boundedText(value, maximum) {
  return typeof value === "string" ? value.slice(0, maximum) : "";
}

// Fingerprints are lowercased here so every comparison across the pairing, sharing and computer
// views is a plain ===.
function fingerprintText(value) {
  return nullableText(value, 128)?.toLowerCase() ?? null;
}

function nullableText(value, maximum) {
  const normalized = boundedText(value, maximum);
  return normalized || null;
}
