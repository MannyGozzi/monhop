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
// every read of it here is optional: a card must render before anything is known.
export function computerStatus(computer, sharingView, active, localName) {
  const fingerprint = computer?.fingerprint ?? null;
  const name = displayName(computer);
  const phase = sharingView?.phase ?? "off";
  const message = sharingView?.message ?? "";
  const live = Boolean(fingerprint) && sharingView?.peerFingerprint === fingerprint;
  if (!live && active !== fingerprint)
    return status("standby", "Paired", "Standby. Use it to share input with it.");
  // An unrecognized reply may still hide a live worker: say what the backend said, without alarm.
  if (phase === "unknown")
    return message ? status("attention", "Needs attention", message) : reaching(name);
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

// The header pill: the computer in use, or why there is nothing to report.
export function activeStatus({ computers, sharingView, active, nativeAvailable, localName }) {
  if (!nativeAvailable)
    return status("preview", "Preview only", "Open MonHop to use these computers.");
  const computer = findComputer(computers, active);
  if (!computer)
    return status(
      "none",
      "No computer",
      computers.items.length ? "Choose a computer to use." : "Pair a computer to get started.",
    );
  return computerStatus(computer, sharingView, active, localName);
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
