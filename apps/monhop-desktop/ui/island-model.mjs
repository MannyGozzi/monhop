// The notch island as the Settings card shows it: a switch on a Mac whose display has a camera notch,
// on until the user turns it off. Rust's view says whether such a display is connected, and the last
// save failure.

export const ISLAND_TITLE = "Notch island";
export const ISLAND_SWITCH_LABEL = "Show MonHop around the notch";
export const ISLAND_DESCRIPTION =
  "Point at the notch for sharing, dimming and clipboard controls. Connection changes show beside it.";

export function normalizeIslandView(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    supported: source.supported === true,
    enabled: source.enabled === true,
    error: typeof source.error === "string" ? source.error : "",
  };
}

// Only on a Mac whose backend answered that a notched display can show the island; anything else
// leaves the card out.
export function islandContext(platform, view, pending) {
  if (platform !== "macos" || view === undefined) return undefined;
  const normalized = normalizeIslandView(view);
  if (!normalized.supported) return undefined;
  return { view: normalized, pending: pending === true };
}
