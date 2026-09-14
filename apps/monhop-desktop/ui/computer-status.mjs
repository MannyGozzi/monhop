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
  error: "error",
};

// The link or the session is up with this computer: the Displays section may be used.
const LIVE_KEYS = new Set(["connected", "editing", "sharing", "reconnecting"]);

// The session is up: sharing, or waiting out a silence without ending it.
const SESSION_KEYS = new Set(["sharing", "reconnecting"]);

const RETRYING = "MonHop keeps trying while both computers are on the same network.";

export function computerStatus(computer, sharingView, active) {
  const fingerprint = computer?.fingerprint ?? null;
  const name = displayName(computer);
  const phase = sharingView?.phase ?? "off";
  const live = Boolean(fingerprint) && sharingView?.peerFingerprint === fingerprint;
  if (!live) {
    if (active !== fingerprint)
      return status("standby", "Paired", "Standby. Use it to share input with it.");
    if (phase === "error" || phase === "unknown")
      return status("error", "Can't connect", sharingView?.message || RETRYING);
    return status("connecting", "Connecting…", `Reaching ${name}.`);
  }
  switch (phase) {
    case "sharing":
      return sharingView.held
        ? status(
            "reconnecting",
            "Reconnecting…",
            "Waiting for the network. Input stays on this computer.",
          )
        : status("sharing", "Sharing input", roleDetail(sharingView.sharingRole, name));
    case "connected":
      return sharingView.editing
        ? status("editing", "Connected", "Arranging displays. Sharing resumes after you apply.")
        : status("connected", "Connected", "Arrange the displays to start sharing.");
    case "stopping":
      return status("stopping", "Stopping…", "Closing the connection.");
    case "error":
    case "unknown":
      return status("error", "Can't connect", sharingView.message || RETRYING);
    default:
      return status("connecting", "Connecting…", `Reaching ${name}.`);
  }
}

// The header pill: the computer in use, or why there is nothing to report.
export function activeStatus({ computers, sharingView, active, nativeAvailable }) {
  if (!nativeAvailable)
    return status("preview", "Preview only", "Open MonHop to use these computers.");
  const computer = findComputer(computers, active);
  if (!computer)
    return status(
      "none",
      "No computer",
      computers.items.length ? "Choose a computer to use." : "Pair a computer to get started.",
    );
  return computerStatus(computer, sharingView, active);
}

export function isLiveStatus(value) {
  return LIVE_KEYS.has(value?.key);
}

export function isSessionStatus(value) {
  return SESSION_KEYS.has(value?.key);
}

// Only the input computer's keyboard and mouse cross; it never stops controlling itself.
function roleDetail(role, name) {
  if (role === "sends") return `Your keyboard and mouse reach ${name}`;
  if (role === "receives") return `${name}'s keyboard and mouse reach this computer`;
  return `One keyboard and mouse across this computer and ${name}`;
}

function status(key, label, detail) {
  return { key, label, detail, tone: TONES[key] ?? "neutral" };
}
