// The notch island as the Settings card shows it: a Mac-only switch, on until the user turns it off.
// Rust's view says whether this Mac can show it, whether its display has a camera notch, and the last
// save failure.

export const ISLAND_TITLE = "Notch island";

export function islandSwitchLabel(notched) {
  return notched ? "Show MonHop around the notch" : "Show MonHop at the top of the screen";
}

export function islandDescription(notched) {
  return notched
    ? "Point at the notch for sharing, dimming and clipboard controls. Connection changes show beside it."
    : "A small island sits in the middle of the menu bar. Point at it for sharing, dimming and clipboard controls.";
}

export function normalizeIslandView(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    supported: source.supported === true,
    enabled: source.enabled === true,
    notched: source.notched === true,
    error: typeof source.error === "string" ? source.error : "",
  };
}

// Only on a Mac whose backend answered that it can show the island; anything else leaves the card out.
export function islandContext(platform, view, pending) {
  if (platform !== "macos" || view === undefined) return undefined;
  const normalized = normalizeIslandView(view);
  if (!normalized.supported) return undefined;
  return { view: normalized, pending: pending === true };
}
