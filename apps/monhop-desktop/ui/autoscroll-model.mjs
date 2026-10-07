// Middle-click autoscroll as the Home card shows it: a Mac-only switch that matters when a Windows
// mouse controls this Mac, on unless Rust says off.

import { normalizeSwitchView, switchContext } from "./switch-model.mjs";

export const AUTOSCROLL_LABEL = "Middle-click autoscroll";
export const AUTOSCROLL_NOTE = "Works when a Windows mouse controls this Mac.";

export function normalizeAutoscrollView(value) {
  return normalizeSwitchView(value, true);
}

export function autoscrollContext(platform, view, pending) {
  return switchContext(platform, "macos", view, pending, true);
}
