// A link that keeps stalling makes MonHop send pointer updates less often until it steadies. The
// card says so, and names this computer's Wi-Fi only when it shares over Wi-Fi, the likely cause.

import { isWifiInterface } from "./model.mjs";

// A link degraded or bad this long earns the note, so one stall never flashes it.
export const LINK_NOTE_AFTER_MS = 2000;

const UNSTEADY = new Set(["degraded", "bad"]);
const SLOWER = "MonHop is sending pointer updates less often until it steadies.";

export const WIFI_UNSTABLE_COPY = `This computer's Wi-Fi is unstable. ${SLOWER}`;

export function connectionUnstableCopy(name) {
  return `The connection to ${name} is unstable. ${SLOWER}`;
}

// The note for a computer's live link, from its `peers[]` entry; null while the link is steady.
export function linkWarning(peer, network, name) {
  if (!UNSTEADY.has(peer?.linkQuality)) return null;
  const unsteady = peer.linkUnsteadyMs;
  if (typeof unsteady !== "number" || unsteady < LINK_NOTE_AFTER_MS) return null;
  return isWifiInterface(network) ? WIFI_UNSTABLE_COPY : connectionUnstableCopy(name);
}

// One note at most: AirDrop names its cause outright, so it outranks the general one.
export function networkNote(airdrop, link) {
  return airdrop || link || null;
}
