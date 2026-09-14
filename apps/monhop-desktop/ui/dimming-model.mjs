// Screen dimming as the window shows it: the saved preference plus whether the overlay is up.

export const DIM_LEVEL_STEP = 1;

export function initialDimming() {
  return {
    view: null,
    pending: false,
    failure: "",
  };
}

// A view from Rust replaces the draft; a failure keeps the last view so the controls stay usable.
export function applyDimmingView(current, view) {
  return { ...current, view, pending: false, failure: "" };
}

export function failDimming(current, failure) {
  return { ...current, pending: false, failure };
}

export function beginDimming(current) {
  return { ...current, pending: true, failure: "" };
}

export function dimButtonLabel(view) {
  return view?.dimmed ? "Undim" : "Dim now";
}

export function levelLabel(level) {
  return `${level}%`;
}

export function shortcutDescription(view) {
  if (!view) return "Toggles the dimming from any app.";
  return view.enabled
    ? `${view.shortcut} toggles the dimming from any app.`
    : `Off. ${view.shortcut} does nothing until you turn it on.`;
}

// The card's own error line: Rust's last failure first, then a failed request from this window.
export function dimmingMessage(dimming) {
  return dimming.failure || dimming.view?.error || "";
}

export function clampLevel(view, level) {
  const min = view?.minLevel ?? 10;
  const max = view?.maxLevel ?? 99;
  const stepped = Math.round(level / DIM_LEVEL_STEP) * DIM_LEVEL_STEP;
  return Math.min(max, Math.max(min, stepped));
}
