// The updater as the window shows it: normalize Rust's view, then derive text and button gates from it.

const PHASES = new Set([
  "idle",
  "checking",
  "upToDate",
  "available",
  "downloading",
  "ready",
  "failed",
]);
const BUSY_PHASES = new Set(["checking", "downloading"]);
const SHARING_HINT = "Sharing is running. Turn it off first or quit MonHop to update.";
const CHECK_FAILED = "The update check did not finish.";

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
    automatic: source.automatic !== false,
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
    case "failed":
      return view.message || CHECK_FAILED;
    default:
      return view.checkedSecondsAgo == null
        ? "Not checked yet."
        : `Checked ${minutesAgo(view.checkedSecondsAgo)} minutes ago.`;
  }
}

export function canCheck(view) {
  return !BUSY_PHASES.has(view.phase);
}

export function canInstall(view) {
  return view.phase === "ready" && !view.sharingActive;
}

// The reason "Restart to update" is disabled, shown under it; empty when installing is allowed.
export function installHint(view) {
  return view.phase === "ready" && view.sharingActive ? SHARING_HINT : "";
}
