// Clipboard sharing as the window shows it: normalize Rust's view (kinds, sizes and fingerprints,
// never clipboard content — the wire shape from settings.rs::ClipboardView does not carry any),
// then derive the status text the Home card renders.

import { displayName, findComputer } from "./computers-model.mjs";

const ACCESS = new Set(["allowed", "ask", "denied", "unknown"]);
const NOTICES = new Set(["tooLarge", "unsupported", "accessDenied"]);
const DIRECTIONS = new Set(["sent", "received"]);
const KINDS = new Set(["text", "image"]);
const FINGERPRINT = /^[a-f0-9]{64}$/i;
const MAX_PEERS = 8;

export const CLIPBOARD_PRIVACY =
  "Copied text and images go only to computers you're sharing with, encrypted over the same " +
  "verified connection as your keyboard and mouse. Nothing leaves your network, and files and " +
  "password-manager items are never sent.";

function cleanFingerprint(value) {
  return typeof value === "string" && FINGERPRINT.test(value) ? value.toLowerCase() : null;
}

function clampedInt(value, min, max) {
  const n = Number(value);
  if (!Number.isFinite(n)) return min;
  return Math.min(max, Math.max(min, Math.round(n)));
}

function normalizePeers(value) {
  if (!Array.isArray(value)) return [];
  const peers = [];
  const seen = new Set();
  for (const entry of value) {
    if (peers.length >= MAX_PEERS) break;
    const fingerprint = cleanFingerprint(entry?.fingerprint);
    if (!fingerprint || seen.has(fingerprint)) continue;
    seen.add(fingerprint);
    const raw = entry?.peerEnabled;
    peers.push({ fingerprint, peerEnabled: raw === true ? true : raw === false ? false : null });
  }
  return peers;
}

function normalizeLast(value) {
  if (!value || typeof value !== "object") return null;
  const direction = DIRECTIONS.has(value.direction) ? value.direction : null;
  const kind = KINDS.has(value.kind) ? value.kind : null;
  const peer = cleanFingerprint(value.peer);
  // Any field the sender didn't recognize (direction/kind/peer) drops the whole entry rather
  // than guessing, since a partial "last transfer" line would misreport what happened.
  if (!direction || !kind || !peer) return null;
  return {
    direction,
    kind,
    bytes: clampedInt(value.bytes, 0, Number.MAX_SAFE_INTEGER),
    peer,
    ageSeconds: clampedInt(value.ageSeconds, 0, Number.MAX_SAFE_INTEGER),
  };
}

// Unknown access/notice values collapse to their safe defaults, peers and the last transfer are
// bounded and reshaped into only the fields above, so a malformed, partial, or hostile payload
// still renders a usable card instead of throwing mid-render — and never carries anything past
// kind, size, and identity through to the screen.
export function normalizeClipboardView(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    enabled: source.enabled === true,
    access: ACCESS.has(source.access) ? source.access : "unknown",
    notice: NOTICES.has(source.notice) ? source.notice : null,
    peers: normalizePeers(source.peers),
    last: normalizeLast(source.last),
  };
}

// The switch shows on and off; this only says what it cannot: on, with nobody to share with yet.
export function clipboardStatusText(view) {
  return view.enabled && !view.peers.length
    ? "Waiting for a computer you're sharing input with."
    : null;
}

// One line per attached peer, named from the paired-computers list so it reads "<name>: on"
// rather than a bare fingerprint.
export function clipboardPeerLines(view, computers) {
  return view.peers.map((peer) => {
    const name = displayName(findComputer(computers ?? { items: [] }, peer.fingerprint));
    return peer.peerEnabled === true ? `${name}: on` : `${name}: off. Turn it on there too`;
  });
}

const ACCESS_NOTICE = {
  ask: "MonHop needs your permission to read what you copy. Allow MonHop in System Settings.",
  denied:
    "MonHop can't read what you copy until you allow it. Allow MonHop in System Settings to " +
    "share it again.",
};

// macOS gates programmatic pasteboard reads behind a permission the user grants once; Windows
// has no such prompt, so `access` stays "allowed" or "unknown" there and this returns null.
export function clipboardAccessNotice(view) {
  return ACCESS_NOTICE[view.access] ?? null;
}

const SKIP_NOTICE = {
  tooLarge: "The last thing you copied was too large to share.",
  unsupported: "The last thing you copied isn't a format MonHop can share.",
  accessDenied: "The last thing you copied wasn't shared: MonHop doesn't have clipboard access.",
};

export function clipboardNoticeText(view) {
  return SKIP_NOTICE[view.notice] ?? null;
}

function humanBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = -1;
  do {
    value /= 1024;
    unit += 1;
  } while (value >= 1024 && unit < units.length - 1);
  const rounded = value < 10 ? Math.round(value * 10) / 10 : Math.round(value);
  return `${rounded} ${units[unit]}`;
}

function ageText(seconds) {
  if (seconds < 60) return `${seconds} s ago`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  return `${Math.round(minutes / 60)} h ago`;
}

const KIND_PHRASE = { text: "text", image: "an image" };
const DIRECTION_VERB = { sent: "Sent", received: "Received" };

// "Sent an image (1.4 MB) 12 s ago" — direction, kind, and size only, never the peer's name or
// anything from the content itself (the normalized view has nothing left to leak here).
export function lastTransferText(view) {
  const { last } = view;
  if (!last) return null;
  return (
    `${DIRECTION_VERB[last.direction]} ${KIND_PHRASE[last.kind]} ` +
    `(${humanBytes(last.bytes)}) ${ageText(last.ageSeconds)}`
  );
}
