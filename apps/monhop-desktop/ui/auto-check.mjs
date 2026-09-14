const INCOMPLETE_CHECK_MESSAGE = "Some checks did not finish. Use refresh to try again.";

export function shouldStartAutomaticSnapshot({
  uiCheck,
  nativeAvailable,
  busy,
  checking,
  automaticFailed,
  trigger,
} = {}) {
  if (uiCheck || !nativeAvailable || busy || checking || automaticFailed) return false;
  if (trigger === "startup" || trigger === "focus") return true;
  return trigger === "entry";
}

export function snapshotCheckResult(snapshot, freshness) {
  if (Array.isArray(snapshot?.errors) && snapshot.errors.length > 0) {
    return { automaticFailed: true, freshness: INCOMPLETE_CHECK_MESSAGE };
  }
  return { automaticFailed: false, freshness };
}

export function canOpenPairingOnEntry({
  uiCheck,
  nativeAvailable,
  busy,
  eligible,
  contextKey,
  attemptedKeys,
} = {}) {
  return (
    !uiCheck &&
    nativeAvailable === true &&
    busy !== true &&
    eligible === true &&
    typeof contextKey === "string" &&
    contextKey.length > 0 &&
    !attemptedKeys?.has(contextKey)
  );
}

export function recordPairingOpenContext(attemptedKeys, contextKey) {
  return typeof contextKey === "string" && contextKey.length > 0
    ? new Set([...(attemptedKeys ?? []), contextKey])
    : new Set(attemptedKeys ?? []);
}

export function selectedNetworkContextKey(selected) {
  if (!selected) return "";
  return JSON.stringify([
    selected.id,
    selected.address,
    selected.prefixLength,
    selected.networkName,
    selected.attachmentKnown,
    selected.up,
    selected.physical,
  ]);
}
