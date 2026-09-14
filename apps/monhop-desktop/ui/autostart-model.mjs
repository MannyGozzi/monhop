// Starting MonHop at login as the window shows it: normalize Rust's view, then derive text per state.

const STATES = new Set([
  "on",
  "off",
  "pending",
  "requiresApproval",
  "disabledBySystem",
  "notFound",
  "failed",
]);
const OPEN_SETTINGS_STATES = new Set(["requiresApproval", "disabledBySystem"]);
const FAILED_FALLBACK = "Could not change how MonHop starts at login.";

const STATUS_TEXT = {
  on: "Starts at login.",
  off: "Off.",
  pending: "Registers after you apply a layout.",
  requiresApproval: "Waiting for your approval in System Settings > General > Login Items.",
  disabledBySystem: "Turned off in Windows Settings > Apps > Startup.",
  notFound: "MonHop moved since it was registered. Turn the switch off and on again.",
};

function text(value, fallback = "") {
  return typeof value === "string" ? value : fallback;
}

// An unrecognized state collapses to "off" and every field gets a safe default, so a malformed or
// partial payload from Rust still renders a usable switch instead of throwing mid-render.
export function normalizeAutostartView(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    enabled: source.enabled !== false,
    state: STATES.has(source.state) ? source.state : "off",
    paired: source.paired === true,
    message: text(source.message),
  };
}

export function autostartStatusText(view) {
  if (view.state === "failed") return view.message || FAILED_FALLBACK;
  return STATUS_TEXT[view.state];
}

// "Open Login Items" / "Open Startup settings" only helps when the OS itself needs the user.
export function showAutostartOpenSettings(view) {
  return OPEN_SETTINGS_STATES.has(view.state);
}

export function autostartOpenLabel(platform) {
  return platform === "windows" ? "Open Startup settings" : "Open Login Items";
}

export function autostartDescription(platform) {
  return platform === "windows"
    ? "MonHop starts in the system tray at login and reconnects to this computer."
    : "MonHop starts in the menu bar at login and reconnects to this computer.";
}
