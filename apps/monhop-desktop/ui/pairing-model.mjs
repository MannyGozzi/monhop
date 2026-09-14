const MAX_PAIRING_CODE_LENGTH = 6200;

const PHASES = new Set([
  "closed",
  "identity-missing",
  "ready",
  "review",
  "requesting-network",
  "waiting",
  "connecting",
  "saving",
  "paired",
  "stopping",
  "error",
]);
const ROLES = new Set(["listen", "connect"]);
const PLATFORMS = new Set(["macos", "windows"]);
const STORAGE_OUTCOMES = new Set(["unchanged", "unverified", "verified"]);
const FINGERPRINT = /^[0-9a-fA-F]{64}$/;

export function initialPairingState() {
  return {
    candidateCode: "",
    candidateStale: false,
    compared: false,
    message: "",
    view: null,
  };
}

export function applyPairingView(state, value) {
  const view = normalizePairingView(value);
  const changedCandidate = state.view?.candidateId !== view.candidateId;
  return {
    ...state,
    candidateStale: changedCandidate ? false : state.candidateStale,
    compared: changedCandidate ? false : state.compared,
    message: "",
    view,
  };
}

export function editCandidateCode(state, value) {
  const candidateCode = boundedText(value, MAX_PAIRING_CODE_LENGTH);
  return {
    ...state,
    candidateCode,
    candidateStale: state.view?.candidateId !== null && state.view?.candidateId !== undefined,
    compared: false,
    message: "",
  };
}

export function invalidatePairingCandidate(state) {
  return {
    ...state,
    candidateStale: state.view?.candidateId !== null && state.view?.candidateId !== undefined,
    compared: false,
  };
}

export function setFingerprintCompared(state, compared) {
  return { ...state, compared: compared === true };
}

export function pairingFailure(state, message) {
  return { ...state, message: boundedText(message, 1000) || "Pairing did not finish. Try again." };
}

export function canInspectPairing(state) {
  return (
    ["ready", "review"].includes(state.view?.phase) &&
    state.candidateCode.trim().length > 0 &&
    !state.view.busy
  );
}

export function canConfirmPairing(state) {
  const view = state.view;
  if (
    !view ||
    view.busy ||
    !Number.isSafeInteger(view.candidateId) ||
    state.candidateStale ||
    !ROLES.has(view.role) ||
    !hasFullFingerprint(view.localFingerprint) ||
    !hasFullFingerprint(view.peerFingerprint)
  )
    return false;
  return view.phase === "review" && state.compared;
}

export function isBusyPairing(state) {
  return (
    state.view?.busy === true ||
    ["requesting-network", "waiting", "connecting", "saving", "stopping"].includes(
      state.view?.phase,
    )
  );
}

export function formatFingerprint(value) {
  if (!hasFullFingerprint(value)) return "Not reported";
  return value.match(/.{1,4}/g).join(" ");
}

function hasFullFingerprint(value) {
  return typeof value === "string" && FINGERPRINT.test(value);
}

export function normalizePairingView(value) {
  const source = value && typeof value === "object" ? value : {};
  const incompleteTrust =
    source.phase === "paired" &&
    (source.storageOutcome !== "verified" ||
      !hasFullFingerprint(source.localFingerprint) ||
      !hasFullFingerprint(source.peerFingerprint));
  const phase = !incompleteTrust && PHASES.has(source.phase) ? source.phase : "error";
  const message = incompleteTrust
    ? "The saved pairing is incomplete. Reload pairing before continuing."
    : boundedText(source.message, 1000);
  return {
    phase,
    localCode: nullableText(source.localCode, MAX_PAIRING_CODE_LENGTH),
    localFingerprint: fingerprintText(source.localFingerprint),
    peerFingerprint: fingerprintText(source.peerFingerprint),
    peerAddress: nullableText(source.peerAddress, 256),
    candidateId: positiveIntegerOrNull(source.candidateId),
    role: ROLES.has(source.role) ? source.role : null,
    storageOutcome: STORAGE_OUTCOMES.has(source.storageOutcome)
      ? source.storageOutcome
      : "unverified",
    message:
      message || (phase === "error" ? "The pairing state was not recognized. Reload pairing." : ""),
    busy: source.busy === true,
    networkAccess: ["not-verified", "requesting", "attempted", "incomplete"].includes(
      source.networkAccess,
    )
      ? source.networkAccess
      : "not-verified",
    networkAccessMessage: boundedText(source.networkAccessMessage, 1000),
    localPlatform: PLATFORMS.has(source.localPlatform) ? source.localPlatform : null,
    peerPlatform: PLATFORMS.has(source.peerPlatform) ? source.peerPlatform : null,
  };
}

export function platformLabel(platform, local = false, short = false) {
  const name =
    platform === "macos"
      ? "Mac"
      : platform === "windows"
        ? short
          ? "Windows"
          : "Windows PC"
        : "computer";
  return local ? `This ${name}` : name;
}

function boundedText(value, maximum) {
  return typeof value === "string" ? value.slice(0, maximum) : "";
}

// Fingerprints are lowercased here so every comparison across the pairing, sharing and computer
// views is a plain ===.
function fingerprintText(value) {
  return nullableText(value, 128)?.toLowerCase() ?? null;
}

function nullableText(value, maximum) {
  const normalized = boundedText(value, maximum);
  return normalized || null;
}

function positiveIntegerOrNull(value) {
  return Number.isSafeInteger(value) && value > 0 ? value : null;
}

export function localNetworkStatus(state) {
  const view = state.view;
  if (view?.phase === "paired" && !state.candidateStale) {
    return {
      label: "Pairing verified",
      detail: "The last pairing exchange worked. This is not a live permission check.",
    };
  }
  if (state.candidateStale)
    return {
      label: "Checked when pairing",
      detail: "Inspect the updated code before requesting access.",
    };
  const label =
    { requesting: "Requesting", attempted: "Request attempted", incomplete: "Request incomplete" }[
      view?.networkAccess
    ] || "Checked when pairing";
  return {
    label,
    detail:
      view?.networkAccessMessage ||
      "Choose a network, then compare fingerprints while pairing to request access. Waiting alone may not show a macOS prompt.",
  };
}
