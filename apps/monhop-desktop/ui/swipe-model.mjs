// Swipe between pages as the Home card shows it: a Mac-only switch that matters when this Mac's
// trackpad controls another computer. Rust's view carries the switch and its last save failure.

import { autoscrollContext } from "./autoscroll-model.mjs";

export const SWIPE_LABEL = "Swipe between pages";
export const SWIPE_NOTE = "A quick two-finger swipe goes back or forward on the other computer.";

// The same Mac-only switch as autoscroll's, on unless Rust says off, so it shares its rules.
export function swipeContext(platform, view, pending) {
  return autoscrollContext(platform, view, pending);
}
