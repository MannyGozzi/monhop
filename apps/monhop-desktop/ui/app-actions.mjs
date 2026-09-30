// Pure logic behind the small pieces of app.js that talk to several enabled computers and the
// clipboard switch, kept apart from app.js's DOM/render machinery so it is checked without a
// browser: the fallback from `sharing_set_enabled` to today's `sharing_set_active` against a
// backend built before the N-computer commands landed, which computer to poll sharing status for,
// and the shape of the clipboard card's context.

// A backend without `sharing_set_enabled` yet rejects the call with Tauri's own dispatch error,
// which names the unmatched command and says it was not found; anything else — a validation
// failure, a network error — is a real failure from a backend that does have the command, and
// must not be swallowed into a silent fallback that hides it.
export function isUnknownCommandError(error, command) {
  const text = errorText(error);
  return /\bnot found\b/i.test(text) && text.includes(command);
}

function errorText(error) {
  return error instanceof Error && error.message ? error.message : String(error ?? "");
}

// Today's single-active-computer payload, replicating exactly what `useComputer` has always sent:
// the fingerprint to switch to, or null to switch off.
export function legacySetActivePayload(fingerprint, enabled, interfaceId) {
  return { fingerprint: enabled ? fingerprint : null, interfaceId };
}

// Turns one computer on or off. Tries the N-computer command first; only a rejection that is
// specifically `sharing_set_enabled` itself not existing on the other end falls back to the
// single-computer command, so the app keeps working against an older backend without ever masking
// a real failure from a newer one.
export async function invokeSetComputerEnabled(invoke, fingerprint, enabled, interfaceId) {
  try {
    return await invoke("sharing_set_enabled", { fingerprint, enabled, interfaceId });
  } catch (error) {
    if (!isUnknownCommandError(error, "sharing_set_enabled")) throw error;
    return await invoke(
      "sharing_set_active",
      legacySetActivePayload(fingerprint, enabled, interfaceId),
    );
  }
}

// Which computer `shouldPollSharing`'s legacy `activeComputer` parameter should be given: the bare
// `active` fingerprint the backend has always sent wins when set (today's single-computer meaning,
// "the one computer in use"), and the first switched-in computer from the newer `enabled` list
// covers a backend that sends `enabled` but never populated `active` — so polling starts even
// before the first `sharing_status` reply has its own `view.enabled` to fall back on.
export function pollFingerprint(computers) {
  return computers?.active ?? computers?.enabled?.[0] ?? null;
}

// The clipboard card's context: undefined until something has proven the backend has clipboard
// sharing at all (a successful status read or a "clipboard" event), so a pre-clipboard app.js and
// an app.js talking to a pre-clipboard backend both leave `ctx.clipboard` unset and the card hidden.
export function clipboardContext(view, pending) {
  return view === undefined ? undefined : { view, pending };
}
