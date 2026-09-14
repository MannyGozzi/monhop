const PHASES = new Set(["prepared", "starting", "active", "stopping", "finished", "error"]);
const TERMINAL_PHASES = new Set(["finished", "error"]);
const MAX_REMAINING_SECONDS = 60;
const MAX_COUNTER_DIGITS = 20;
const MAX_MESSAGE_LENGTH = 600;
const MAX_PEER_NAME_LENGTH = 48;
const DECIMAL_COUNT = /^\d+$/;

export function initialTrialState(nativeAvailable) {
  const available = nativeAvailable === true;
  return {
    nativeAvailable: available,
    phase: "prepared",
    message: available
      ? "Ready when both computers choose Start."
      : "Open MonHop to run this controlled test.",
    remainingSeconds: 60,
    sentEvents: "0",
    receivedEvents: "0",
    nativeArrivals: { keyboard: 0, button: 0, movement: 0, scroll: 0 },
    busy: false,
    retryableStart: available,
    preflightFailure: false,
    sendsInput: null,
    peerName: "the other computer",
    startRequestPending: false,
    startConsumed: false,
    statusLost: false,
    closePending: false,
    closeError: "",
  };
}

export function normalizeTrialView(value) {
  const source = value && typeof value === "object" ? value : null;
  if (
    !source ||
    !PHASES.has(source.phase) ||
    typeof source.busy !== "boolean" ||
    typeof source.retryableStart !== "boolean" ||
    typeof source.preflightFailure !== "boolean"
  )
    return null;
  const remainingSeconds = remainingSecondsValue(source.remainingSeconds);
  const sentEvents = counterString(source.sentEvents);
  const receivedEvents = counterString(source.receivedEvents);
  const nativeArrivals = source.nativeArrivals;
  if (
    remainingSeconds === null ||
    sentEvents === null ||
    receivedEvents === null ||
    !nativeArrivals ||
    !["keyboard", "button", "movement", "scroll"].every(
      (key) => Number.isSafeInteger(nativeArrivals[key]) && nativeArrivals[key] >= 0,
    )
  )
    return null;
  if (
    (source.retryableStart && (source.phase !== "prepared" || source.busy)) ||
    (source.preflightFailure && !source.retryableStart)
  )
    return null;
  if (source.sendsInput !== undefined && typeof source.sendsInput !== "boolean") return null;
  if (source.peerName !== undefined && typeof source.peerName !== "string") return null;
  return {
    phase: source.phase,
    message: boundedMessage(source.message),
    remainingSeconds,
    sentEvents,
    receivedEvents,
    nativeArrivals: { ...nativeArrivals },
    busy: source.busy,
    retryableStart: source.retryableStart,
    preflightFailure: source.preflightFailure,
    sendsInput: source.sendsInput === undefined ? null : source.sendsInput,
    peerName: peerNameValue(source.peerName),
  };
}

function peerNameValue(value) {
  const name =
    typeof value === "string"
      ? value
          .replace(/[\p{Cc}]/gu, "")
          .trim()
          .slice(0, MAX_PEER_NAME_LENGTH)
      : "";
  return name || "the other computer";
}

export function applyTrialView(state, value) {
  return applyView(state, value, false);
}

export function applyTrialStartResult(state, value) {
  return applyView(state, value, true);
}

function applyView(state, value, settlesStartRequest) {
  if (state.startRequestPending && !settlesStartRequest) return state;
  const view = normalizeTrialView(value);
  if (!view)
    return statusLost(
      state,
      "The test status was not recognized. Close the test window and open it again.",
    );
  return {
    ...state,
    ...view,
    startConsumed: view.retryableStart ? false : state.startConsumed || view.phase !== "prepared",
    startRequestPending: settlesStartRequest ? false : state.startRequestPending,
    statusLost: false,
    closeError: "",
  };
}

export function beginTrialStart(state) {
  if (!canStartTrial(state)) return state;
  return {
    ...state,
    phase: "starting",
    message: "Waiting for both computers to start.",
    busy: true,
    retryableStart: false,
    preflightFailure: false,
    startRequestPending: true,
    startConsumed: true,
    closeError: "",
  };
}

export function beginTrialStop(state) {
  if (!canStopTrial(state)) return state;
  return {
    ...state,
    phase: "stopping",
    message: "Stopping the controlled test.",
    busy: true,
    retryableStart: false,
    preflightFailure: false,
    startRequestPending: false,
    startConsumed: true,
    closeError: "",
  };
}

export function beginTrialClose(state) {
  if (!canReturnToSetup(state)) return state;
  return {
    ...state,
    phase: "stopping",
    message: "Returning to setup. Releasing held input before closing.",
    busy: true,
    retryableStart: false,
    preflightFailure: false,
    startRequestPending: false,
    startConsumed: true,
    closePending: true,
    closeError: "",
  };
}

export function closeFailed(state, message) {
  return {
    ...state,
    phase: "error",
    message:
      boundedMessage(message) || "Could not return to setup. Cleanup could not be confirmed.",
    busy: false,
    retryableStart: false,
    preflightFailure: false,
    startRequestPending: false,
    startConsumed: true,
    statusLost: true,
    closePending: false,
    closeError: boundedMessage(message),
  };
}

export function statusLost(state, message) {
  return {
    ...state,
    phase: "error",
    message:
      boundedMessage(message) ||
      "The test status was lost. Close the test window and open it again.",
    busy: false,
    retryableStart: false,
    preflightFailure: false,
    startRequestPending: false,
    startConsumed: true,
    statusLost: true,
    closePending: false,
    closeError: "",
  };
}

export function canStartTrial(state) {
  return (
    state.nativeAvailable &&
    state.phase === "prepared" &&
    !state.busy &&
    state.retryableStart &&
    !state.startRequestPending &&
    !state.startConsumed &&
    !state.statusLost &&
    !state.closePending
  );
}

export function canStopTrial(state) {
  return (
    state.nativeAvailable &&
    !state.closePending &&
    (state.busy || !TERMINAL_PHASES.has(state.phase) || state.statusLost)
  );
}

export function canReturnToSetup(state) {
  return state.nativeAvailable && !state.closePending;
}

export function canNavigateTrialControls(state) {
  if (state.closePending || state.statusLost || state.busy || state.startRequestPending)
    return false;
  return (
    (state.phase === "prepared" && state.retryableStart && !state.startConsumed) ||
    TERMINAL_PHASES.has(state.phase)
  );
}

export function shouldPollTrial(state) {
  return (
    state.nativeAvailable &&
    !state.closePending &&
    !state.statusLost &&
    !state.startRequestPending &&
    (state.busy || (!TERMINAL_PHASES.has(state.phase) && state.startConsumed))
  );
}

function remainingSecondsValue(value) {
  return Number.isSafeInteger(value) && value >= 0 && value <= MAX_REMAINING_SECONDS ? value : null;
}

function counterString(value) {
  return typeof value === "string" &&
    value.length <= MAX_COUNTER_DIGITS &&
    DECIMAL_COUNT.test(value)
    ? value
    : null;
}

function boundedMessage(value) {
  return typeof value === "string" ? value.slice(0, MAX_MESSAGE_LENGTH) : "";
}
