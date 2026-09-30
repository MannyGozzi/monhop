// Middle-click autoscroll as the Home card shows it: a Mac-only switch that matters when a Windows
// mouse controls this Mac. Rust's view carries the switch and its last save failure.

export const AUTOSCROLL_LABEL = "Middle-click autoscroll";
export const AUTOSCROLL_NOTE = "Works when a Windows mouse controls this Mac.";

// On unless Rust says off, matching the saved default; anything malformed still renders a switch.
export function normalizeAutoscrollView(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    enabled: source.enabled !== false,
    error: typeof source.error === "string" ? source.error : "",
  };
}

// The card's context: only on a Mac and only once Rust has answered, so another platform or an
// older backend leaves the card out.
export function autoscrollContext(platform, view, pending) {
  if (platform !== "macos" || view === undefined) return undefined;
  return { view: normalizeAutoscrollView(view), pending: pending === true };
}
