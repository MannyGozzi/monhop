// AirDrop set to Everyone keeps a Mac's Wi-Fi hopping to Apple's peer-to-peer channel a few times a
// second, so shared input over that Wi-Fi stalls and jumps. MonHop cannot change it; it says so.

import { isWifiInterface } from "./model.mjs";

export const AIRDROP_STUTTER_COPY =
  "AirDrop is set to Everyone, which pauses this Mac's Wi-Fi a few times a second and makes the pointer stutter. Set AirDrop to Contacts Only in Control Center, or use a cable.";

// While the warning shows, how often MonHop asks again so it leaves once AirDrop changes.
export const AIRDROP_RECHECK_MS = 3000;

export function normalizeAirDropView(value) {
  return { everyone: value?.everyone === true };
}

// The warning for a Mac sharing over Wi-Fi with AirDrop open to everyone; null otherwise.
export function airdropWarning(platform, view, network) {
  if (platform !== "macos" || view?.everyone !== true || !isWifiInterface(network)) return null;
  return AIRDROP_STUTTER_COPY;
}
