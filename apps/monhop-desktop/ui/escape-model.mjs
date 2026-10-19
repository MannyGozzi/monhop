// The emergency stop as the status bar and Settings both show it.

export const ESCAPE_SENTENCE =
  "Hold both Control keys and Esc for 2 seconds to take your keyboard and mouse back.";

// The keys as this computer's keyboard labels them.
export function escapeKeys(platform) {
  return platform === "windows" ? ["Ctrl", "Ctrl", "Esc"] : ["control", "control", "esc"];
}
