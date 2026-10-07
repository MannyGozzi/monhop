// A saved on-or-off switch as a Home card shows it: Rust's view carries the switch and its last save
// failure, and anything malformed still renders a switch at its default.

export function normalizeSwitchView(value, enabledByDefault) {
  const source = value && typeof value === "object" ? value : {};
  return {
    enabled: typeof source.enabled === "boolean" ? source.enabled : enabledByDefault,
    error: typeof source.error === "string" ? source.error : "",
  };
}

// The card's context: only on `shownOn` and only once Rust has answered, so another platform or an
// older backend leaves the card out.
export function switchContext(platform, shownOn, view, pending, enabledByDefault) {
  if (platform !== shownOn || view === undefined) return undefined;
  return { view: normalizeSwitchView(view, enabledByDefault), pending: pending === true };
}
