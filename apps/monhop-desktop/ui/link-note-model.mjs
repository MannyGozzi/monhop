// A link that keeps stalling makes MonHop send pointer updates less often until it steadies. The
// card says so, and names Wi-Fi only when this computer shares over it: the other side may be the
// cause, so neither note says whose fault it is.

import { isWifiInterface } from "./model.mjs";

// Trouble that keeps arriving this long earns the note, so one stall never flashes it.
export const LINK_NOTE_AFTER_MS = 2000;

const KNOWN_QUALITY = new Set(["healthy", "degraded", "bad"]);
const CAPPED = new Set(["degraded", "bad"]);
const SLOWER = "MonHop is sending pointer updates less often until it steadies.";

export const WIFI_UNSTABLE_COPY = "Wi-Fi looks unstable.";
export const WIFI_UNSTABLE_SLOWER_COPY = `${WIFI_UNSTABLE_COPY} ${SLOWER}`;

export function connectionUnstableCopy(name) {
  return `The connection to ${name} looks unstable.`;
}

// Only a capped link may say it sends less often: a radio that keeps leaving can read unsteady
// while every update still goes out.
export function linkWarning(peer, network, name) {
  if (!KNOWN_QUALITY.has(peer?.linkQuality)) return null;
  const unsteady = peer.linkUnsteadyMs;
  if (typeof unsteady !== "number" || unsteady < LINK_NOTE_AFTER_MS) return null;
  const unstable = isWifiInterface(network) ? WIFI_UNSTABLE_COPY : connectionUnstableCopy(name);
  return CAPPED.has(peer.linkQuality) ? `${unstable} ${SLOWER}` : unstable;
}

// One note at most, the surest cause first: AirDrop is certain, Universal Control likely.
export function networkNote(airdrop, universalControl, link) {
  return airdrop || universalControl || link || null;
}
