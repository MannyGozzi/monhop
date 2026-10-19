// The updater as the window shows it: normalize Rust's view, then derive text and button gates from it.

const PHASES = new Set([
  "idle",
  "checking",
  "upToDate",
  "available",
  "downloading",
  "ready",
  "installing",
  "failed",
]);
const BUSY_PHASES = new Set(["checking", "downloading", "installing"]);
const SHARING_HINT = "Pause sharing to restart now, or it installs when you quit MonHop.";
const CHECK_SHARING_HINT = "Pause sharing to check now.";
const CHECK_FAILED = "The update check did not finish.";

// Under a ready update, wherever Restart to update is offered.
export const UPDATE_READY_NOTE = "It installs when you quit MonHop, or now.";

function text(value, fallback = "") {
  return typeof value === "string" ? value : fallback;
}

function nullableText(value) {
  return typeof value === "string" ? value : null;
}

function clampedInt(value, min, max) {
  const n = Number(value);
  if (!Number.isFinite(n)) return null;
  const rounded = Math.round(n);
  return Math.min(max, Math.max(min, rounded));
}

// Unknown phases collapse to "idle" and every field gets a safe default, so a malformed or
// partial payload from Rust still renders a usable card instead of throwing mid-render.
export function normalizeUpdatesView(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    automatic: source.automatic === true,
    phase: PHASES.has(source.phase) ? source.phase : "idle",
    currentVersion: text(source.currentVersion),
    buildCommit: text(source.buildCommit),
    availableVersion: nullableText(source.availableVersion),
    notes: nullableText(source.notes),
    progressPercent:
      source.progressPercent == null ? null : clampedInt(source.progressPercent, 0, 100),
    message: text(source.message),
    checkedSecondsAgo:
      source.checkedSecondsAgo == null ? null : clampedInt(source.checkedSecondsAgo, 0, Infinity),
    host: text(source.host, "github.com"),
    sharingActive: source.sharingActive === true,
  };
}

function minutesAgo(seconds) {
  return Math.max(1, Math.round(seconds / 60));
}

export function updatesStatusText(view) {
  if (view.message && !BUSY_PHASES.has(view.phase)) return view.message;
  switch (view.phase) {
    case "checking":
      return `Checking ${view.host}…`;
    case "upToDate":
      return "MonHop is up to date.";
    case "available":
      return view.automatic
        ? `MonHop ${view.availableVersion} is available. Downloading…`
        : `MonHop ${view.availableVersion} is available.`;
    case "downloading":
      return `Downloading MonHop ${view.availableVersion}… ${view.progressPercent ?? 0}%`;
    case "ready":
      return `MonHop ${view.availableVersion} is ready to install.`;
    case "installing":
      return `Installing MonHop ${view.availableVersion}…`;
    case "failed":
      return view.message || CHECK_FAILED;
    default:
      return view.checkedSecondsAgo == null
        ? "Not checked yet."
        : `Checked ${minutesAgo(view.checkedSecondsAgo)} minutes ago.`;
  }
}

export function canCheck(view) {
  return !view.sharingActive && !BUSY_PHASES.has(view.phase);
}

export function canInstall(view) {
  return view.phase === "ready" && !view.sharingActive;
}

// The reason "Restart to update" is disabled, shown under it; empty when installing is allowed.
export function installHint(view) {
  return view.phase === "ready" && view.sharingActive ? SHARING_HINT : "";
}

// The reason Check now is disabled when sharing is the only thing in its way; empty otherwise.
export function checkHint(view) {
  return view.sharingActive && view.phase !== "ready" && !BUSY_PHASES.has(view.phase)
    ? CHECK_SHARING_HINT
    : "";
}
