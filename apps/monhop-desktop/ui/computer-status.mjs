import { displayName, findComputer } from "./computers-model.mjs";

// The one place a connection becomes words. The header pill, Home, the Computers list and the
// Displays gate all read this, so a computer can never show two different states at once.
const TONES = {
  standby: "neutral",
  connecting: "checking",
  connected: "connected",
  editing: "connected",
  sharing: "active",
  reconnecting: "checking",
  stopping: "checking",
  paused: "neutral",
  attention: "checking",
  error: "error",
};

// The link or the session is up with this computer: the Displays section may be used.
const LIVE_KEYS = new Set(["connected", "editing", "sharing", "reconnecting"]);

// The session is up: sharing, or waiting out a silence without ending it.
const SESSION_KEYS = new Set(["sharing", "reconnecting"]);

// The view is null until the first status reply lands, and stays null if that call failed, so
// every read of it here is optional: a card must render before anything is known. `sharingView`
// may carry a `peers` list (one entry per computer with a live link or session, keyed by
// fingerprint); when it does, this computer's own entry there is authoritative and the legacy
// `peerFingerprint`/`active`/`held`/`editing` fields below it are ignored for it. A view with no
// `peers` at all, or none naming this computer, falls back to those legacy fields unchanged, so a
// reply from before the multi-computer backend lands reads exactly as it always has.
export function computerStatus(computer, sharingView, active, localName) {
  const fingerprint = computer?.fingerprint ?? null;
  const name = displayName(computer);
  const phase = sharingView?.phase ?? "off";
  const message = sharingView?.message ?? "";
  // An unrecognized reply may still hide a live worker: say what the backend said, without alarm.
  // This is about the reply as a whole, so it is checked before any per-computer lookup.
  if (phase === "unknown")
    return message ? status("attention", "Needs attention", message) : reaching(name);
  const peer = findPeer(sharingView, fingerprint);
  if (peer) return peerStatus(peer, sharingView, name, localName);
  return legacyStatus(fingerprint, name, phase, message, sharingView, active, localName);
}

function findPeer(sharingView, fingerprint) {
  if (!fingerprint || !Array.isArray(sharingView?.peers)) return null;
  return sharingView.peers.find((entry) => entry?.fingerprint === fingerprint) ?? null;
}

// This computer's own entry in `sharingView.peers`: its phase, message and held flag replace the
// view's single legacy set, everything else (control, editing) still comes from the shared view
// since only one arrangement editor and one control record exist across the whole group.
function peerStatus(peer, sharingView, name, localName) {
  if (peer.phase === "notPaired")
    return status(
      "attention",
      "Not paired",
      peer.message || `Pair with ${name} to share input with it.`,
    );
  switch (peer.phase) {
    case "sharing":
      return peer.held
        ? status(
            "reconnecting",
            "Reconnecting…",
            "Waiting for the network. Input stays on this computer.",
          )
        : status("sharing", "Sharing", controlDetail(sharingView?.control, localName, name));
    case "connected":
      return sharingView?.editing
        ? status("editing", "Arranging displays", "Sharing resumes after you apply.")
        : status("connected", "Connected", "Arrange the displays to start sharing.");
    case "off":
      // The link cleared its peer the instant it closed, so this only fires on a stray poll.
      return peer.message ? offStatus(peer.message) : reaching(name);
    case "stopping":
      return status("stopping", "Stopping…", "Closing the connection.");
    case "error":
      // No message means nothing is confirmed wrong yet; that reads as still connecting, not failed.
      return peer.message ? status("error", "Can't connect", peer.message) : reaching(name);
    default:
      return reaching(name);
  }
}

// Today's single-peer reply: `sharingView` itself names the one computer it is live with.
function legacyStatus(fingerprint, name, phase, message, sharingView, active, localName) {
  const live = Boolean(fingerprint) && sharingView?.peerFingerprint === fingerprint;
  if (!live && active !== fingerprint)
    return status("standby", "Paired", "Standby. Use it to share input with it.");
  if (!live) {
    if (phase === "error" && message) return status("error", "Can't connect", message);
    if (phase === "off" && message) return offStatus(message);
    return reaching(name);
  }
  switch (phase) {
    case "sharing":
      return sharingView?.held
        ? status(
            "reconnecting",
            "Reconnecting…",
            "Waiting for the network. Input stays on this computer.",
          )
        : status("sharing", "Sharing", controlDetail(sharingView?.control, localName, name));
    case "connected":
      return sharingView?.editing
        ? status("editing", "Arranging displays", "Sharing resumes after you apply.")
        : status("connected", "Connected", "Arrange the displays to start sharing.");
    case "off":
      // The link cleared its peer the instant it closed, so this only fires on a stray poll.
      return message ? offStatus(message) : reaching(name);
    case "stopping":
      return status("stopping", "Stopping…", "Closing the connection.");
    case "error":
      // No message means nothing is confirmed wrong yet; that reads as still connecting, not failed.
      return message ? status("error", "Can't connect", message) : reaching(name);
    default:
      return reaching(name);
  }
}

// Every message the backend sends with the link down, in its own words. A pause is the user's
// own doing; a message that names a reconnect, a connect, or sharing being on is a step on the
// way back and must never read as a failure; anything else is simply not connected.
const TRANSITIONS = ["Reconnecting", "Connecting", "Sharing is on", "Switching"];

function offStatus(message) {
  if (message.startsWith("Paused")) return status("paused", "Paused", message);
  if (TRANSITIONS.some((word) => message.includes(word)))
    return status("connecting", "Connecting…", message);
  return status("paused", "Not connected", message);
}

function reaching(name) {
  return status("connecting", "Connecting…", `Reaching ${name}.`);
}

// The header pill: the computer (or computers) in use, or why there is nothing to report. With
// zero or one computer enabled this reads exactly as it always has, naming that one computer's own
// status; `enabled` only changes anything once it names more than one, when the pill instead
// summarizes across all of them (sharing, still connecting, or paused).
export function activeStatus({
  computers,
  sharingView,
  active,
  enabled,
  nativeAvailable,
  localName,
}) {
  if (!nativeAvailable)
    return status("preview", "Preview only", "Open MonHop to use these computers.");
  const list = enabledFingerprints(sharingView, enabled, active);
  if (list.length > 1) return groupStatus(list, computers, sharingView, localName);
  const single = list[0] ?? active ?? null;
  const computer = findComputer(computers, single);
  if (!computer)
    return status(
      "none",
      "No computer",
      computers.items.length ? "Choose a computer to use." : "Pair a computer to get started.",
    );
  return computerStatus(computer, sharingView, single, localName);
}

// The live view's own `enabled` list wins once the backend sends one; the caller's own list (from
// the polled computers reply) is next; a bare `active` fingerprint is today's only source and is
// always the fallback, so an unmigrated caller sees exactly the single-computer behavior it always has.
function enabledFingerprints(sharingView, enabled, active) {
  if (Array.isArray(sharingView?.enabled) && sharingView.enabled.length) return sharingView.enabled;
  if (Array.isArray(enabled) && enabled.length) return enabled;
  return active ? [active] : [];
}

// Sharing with every one of them reads as one clean line; a pause is the user's own doing and says
// so; anything else names how many still need attention or are still on their way, the same
// "X of Y" shape wherever a count is short of the total.
function groupStatus(list, computers, sharingView, localName) {
  const total = list.length;
  if (sharingView?.paused === true)
    return status("paused", "Paused", `Paused for ${total} computers.`);
  let sharing = 0;
  let attention = 0;
  for (const fingerprint of list) {
    const computer = findComputer(computers, fingerprint) ?? { fingerprint, platform: null };
    const key = computerStatus(computer, sharingView, fingerprint, localName).key;
    if (key === "sharing") sharing += 1;
    else if (key === "error" || key === "attention") attention += 1;
  }
  if (attention > 0)
    return status("error", "Can't connect", `Can't connect to ${attention} of ${total} computers.`);
  if (sharing === total) return status("sharing", "Sharing", `Sharing with ${total} computers.`);
  const remaining = total - sharing;
  return status(
    "connecting",
    "Connecting…",
    sharing > 0
      ? `Connecting to ${remaining} of ${total} computers.`
      : `Connecting to ${total} computers.`,
  );
}

export function isLiveStatus(value) {
  return LIVE_KEYS.has(value?.key);
}

export function isSessionStatus(value) {
  return SESSION_KEYS.has(value?.key);
}

const BOTH_DIRECTIONS = "Either computer's keyboard and mouse can control the other";

// Either direction can be on: both, or just one. No active record reads as both, matching the
// default a fresh setup turns on.
function controlDetail(control, localName, peerName) {
  const localToPeer = control?.localToPeer ?? true;
  const peerToLocal = control?.peerToLocal ?? true;
  if (localToPeer && peerToLocal) return BOTH_DIRECTIONS;
  if (localToPeer) return `${localName}'s keyboard and mouse can control ${peerName}`;
  if (peerToLocal) return `${peerName}'s keyboard and mouse can control ${localName}`;
  return BOTH_DIRECTIONS;
}

function status(key, label, detail) {
  return { key, label, detail, tone: TONES[key] ?? "neutral" };
}
