// AirDrop set to Everyone keeps a Mac's Wi-Fi hopping to Apple's peer-to-peer channel a few times a
// second, so shared input over that Wi-Fi stalls and jumps. MonHop cannot change it; it says so.

import { isWifiInterface } from "./model.mjs";

export const AIRDROP_STUTTER_COPY =
  "AirDrop is set to Everyone, which pauses this Mac's Wi-Fi a few times a second and makes the pointer stutter. Set AirDrop to Contacts Only in Control Center, or use a cable.";

export const UNIVERSAL_CONTROL_STUTTER_COPY =
  "Wi-Fi keeps pausing on this connection, and Universal Control on this Mac can cause that. If you don't use it, turn off “Allow your pointer and keyboard to move between any nearby Mac or iPad” in System Settings > Displays > Advanced.";

// While a warning shows, how often MonHop asks again so it leaves once the setting changes.
export const AIRDROP_RECHECK_MS = 3000;

export function normalizeAirDropView(value) {
  return {
    everyone: value?.everyone === true,
    universalControl: value?.universalControl === true,
  };
}

// The warning for a Mac sharing over Wi-Fi with AirDrop open to everyone; null otherwise.
export function airdropWarning(platform, view, network) {
  if (platform !== "macos" || view?.everyone !== true || !isWifiInterface(network)) return null;
  return AIRDROP_STUTTER_COPY;
}

// The setting alone is on for most Macs and the hopping alone has other causes, so both must agree.
export function universalControlWarning(platform, view, network, peer) {
  if (platform !== "macos" || view?.universalControl !== true) return null;
  if (!isWifiInterface(network) || peer?.linkHopping !== true) return null;
  return UNIVERSAL_CONTROL_STUTTER_COPY;
}
