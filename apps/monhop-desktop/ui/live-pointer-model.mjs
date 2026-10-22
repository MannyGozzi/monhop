// Show which pointer is live, as Settings > Sharing shows it: on a Mac or a Windows PC once Rust has
// answered, on unless Rust says off.

import { switchContext } from "./switch-model.mjs";

export const LIVE_POINTER_LABEL = "Show which pointer is live";
export const LIVE_POINTER_NOTE =
  "Dims this computer's pointer while it isn't the live one, and rings the pointer where control lands.";

export function livePointerContext(platform, view, pending) {
  if (platform !== "macos" && platform !== "windows") return undefined;
  return switchContext(platform, platform, view, pending, true);
}
