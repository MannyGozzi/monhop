// AirDrop set to Everyone keeps a Mac's Wi-Fi hopping to Apple's peer-to-peer channel a few times a
// second, so shared input over that Wi-Fi stalls and jumps. MonHop cannot change it; it says so.

import { isWifiInterface } from "./model.mjs";

export const AIRDROP_STUTTER_COPY =
  "AirDrop is set to Everyone, which pauses this Mac's Wi-Fi a few times a second and makes the pointer stutter. Set AirDrop to Contacts Only in Control Center, or use a cable.";

const RADIO_AWAY =
  "Wi-Fi keeps pausing about twice a second because a Mac's radio keeps leaving your network to reach another Apple device nearby, which a faster router can't fix.";
const HANDOFF_SWITCH = "Allow Handoff in System Settings > General > AirDrop & Handoff";
const UNIVERSAL_CONTROL_SWITCH =
  "“Allow your pointer and keyboard to move between any nearby Mac or iPad” in System Settings > Displays > Advanced";

export const HANDOFF_STUTTER_COPY = `${RADIO_AWAY} Handoff can cause this: on a Mac that doesn't need it, turn off ${HANDOFF_SWITCH}.`;
export const CONTINUITY_STUTTER_COPY = `${RADIO_AWAY} Handoff or Universal Control can cause this: on a Mac that doesn't need them, turn off ${HANDOFF_SWITCH}, and ${UNIVERSAL_CONTROL_SWITCH}.`;

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

// The beat alone marks a Mac's peer-to-peer radio. Which service holds it cannot be read from here,
// so Universal Control is named only while its setting is on.
export function hoppingWarning(platform, view, network, peer) {
  if (platform !== "macos" || !isWifiInterface(network) || peer?.linkHopping !== true) return null;
  return view?.universalControl === true ? CONTINUITY_STUTTER_COPY : HANDOFF_STUTTER_COPY;
}
