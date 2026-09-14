import {
  applyTrialStartResult,
  applyTrialView,
  beginTrialClose,
  beginTrialStart,
  beginTrialStop,
  canNavigateTrialControls,
  canReturnToSetup,
  canStartTrial,
  canStopTrial,
  closeFailed,
  initialTrialState,
  shouldPollTrial,
  statusLost,
} from "./trial-model.mjs";

const core = window.__TAURI__?.core;
const platform = ["macos", "windows"].includes(window.__MONHOP_PLATFORM__)
  ? window.__MONHOP_PLATFORM__
  : "other";
document.documentElement.dataset.platform = platform;

const phaseNode = document.querySelector("#trial-phase");
const messageNode = document.querySelector("#trial-message");
const failureNode = document.querySelector("#trial-failure");
const failureMessageNode = document.querySelector("#trial-failure-message");
const remainingNode = document.querySelector("#trial-remaining");
const startButton = document.querySelector("#trial-start");
const stopButton = document.querySelector("#trial-stop");
const returnButton = document.querySelector("#trial-return");
const pad = document.querySelector("#input-pad");
const arrivalsNode = document.querySelector("#trial-arrivals");
const landedNode = document.querySelector("#trial-landed");
const protocolNode = document.querySelector("#trial-protocol");
const failureTitleNode = document.querySelector("#trial-failure-title");
const roleNode = document.querySelector("#trial-role");

let trial = initialTrialState(Boolean(core?.invoke));
let actionGeneration = 0;
let pollTimer = null;
let activityTimer = null;
let arrivalTotal = 0;
let closeInFlight = false;
let closeAttempted = false;
const landed = { keyboard: 0, button: 0, movement: 0, scroll: 0 };

startButton.addEventListener("click", startTrial);
stopButton.addEventListener("click", stopTrial);
returnButton.addEventListener("click", returnToSetup);
window.addEventListener("pagehide", closeTrial);

for (const type of ["keydown", "keyup", "keypress"]) {
  document.addEventListener(
    type,
    (event) => {
      if (!allowsControlKey(event)) event.preventDefault();
    },
    { capture: true },
  );
}
pad.addEventListener("wheel", (event) => event.preventDefault(), { passive: false });
// Counts what the window itself received, independent of the native tallies, so a
// mismatch between the two is visible without logging key identities or text.
document.addEventListener("keydown", () => countLanded("keyboard"), { capture: true });
document.addEventListener("pointerdown", () => countLanded("button"), { capture: true });
document.addEventListener("pointermove", () => countLanded("movement"), { capture: true });
document.addEventListener("wheel", () => countLanded("scroll"), { capture: true, passive: true });
for (const type of [
  "contextmenu",
  "dragstart",
  "dragover",
  "drop",
  "selectstart",
  "beforeinput",
  "input",
  "paste",
  "copy",
  "cut",
]) {
  document.addEventListener(type, (event) => event.preventDefault(), { capture: true });
}

render();

async function startTrial() {
  if (!core?.invoke || !canStartTrial(trial)) return;
  const generation = ++actionGeneration;
  trial = beginTrialStart(trial);
  render();
  try {
    const view = await core.invoke("trial_start");
    if (generation !== actionGeneration) return;
    trial = applyTrialStartResult(trial, view);
  } catch (error) {
    if (generation !== actionGeneration) return;
    trial = statusLost(trial, `Could not start the test: ${nativeError(error)}`);
  }
  render();
  schedulePoll();
}

async function stopTrial() {
  if (!core?.invoke || !canStopTrial(trial)) return;
  const generation = ++actionGeneration;
  trial = beginTrialStop(trial);
  render();
  try {
    const view = await core.invoke("trial_stop");
    if (generation !== actionGeneration) return;
    if (view !== undefined) trial = applyTrialView(trial, view);
  } catch (error) {
    if (generation !== actionGeneration) return;
    trial = statusLost(trial, `Could not stop the test: ${nativeError(error)}`);
  }
  render();
  schedulePoll();
}

async function returnToSetup() {
  if (!core?.invoke || !canReturnToSetup(trial) || closeInFlight) return;
  closeInFlight = true;
  closeAttempted = true;
  actionGeneration += 1;
  clearTimers();
  trial = beginTrialClose(trial);
  render();
  try {
    await core.invoke("trial_close");
  } catch (error) {
    closeInFlight = false;
    closeAttempted = false;
    trial = closeFailed(
      trial,
      `Could not return to setup. Cleanup could not be confirmed: ${nativeError(error)}`,
    );
    render();
  }
}

function schedulePoll() {
  if (pollTimer !== null || !shouldPollTrial(trial) || !core?.invoke) return;
  const generation = actionGeneration;
  pollTimer = window.setTimeout(async () => {
    pollTimer = null;
    if (generation !== actionGeneration || !shouldPollTrial(trial) || !core?.invoke) return;
    try {
      const view = await core.invoke("trial_status");
      if (generation !== actionGeneration) return;
      trial = applyTrialView(trial, view);
    } catch (error) {
      if (generation !== actionGeneration) return;
      trial = statusLost(trial, `The test status was lost: ${nativeError(error)}`);
    }
    render();
    schedulePoll();
  }, 250);
}

function closeTrial() {
  if (closeAttempted) return;
  closeAttempted = true;
  actionGeneration += 1;
  clearTimers();
  if (core?.invoke) void core.invoke("trial_close").catch(() => {});
}

function clearTimers() {
  if (pollTimer !== null) window.clearTimeout(pollTimer);
  pollTimer = null;
  if (activityTimer !== null) window.clearTimeout(activityTimer);
  activityTimer = null;
}

function render() {
  document.body.dataset.phase = trial.phase;
  pad.dataset.active = String(trial.phase === "active");
  phaseNode.textContent = phaseLabel(trial);
  roleNode.textContent = roleLabel(trial);
  const failed = trial.phase === "error" || trial.preflightFailure || trial.closeError !== "";
  messageNode.textContent = failed ? "" : trial.message;
  failureNode.hidden = !failed;
  failureTitleNode.textContent = failureTitle(trial);
  failureMessageNode.textContent = failed ? trial.message : "";
  remainingNode.textContent = `${trial.remainingSeconds}s`;
  protocolNode.textContent = `Sent ${trial.sentEvents} · received ${trial.receivedEvents}`;
  const counts = trial.nativeArrivals;
  arrivalsNode.textContent = `Reached MonHop · ${countSummary(counts)}`;
  landedNode.textContent = `Landed in this window · ${countSummary(landed)}`;
  const total = Object.values(counts).reduce((sum, value) => sum + value, 0);
  if (total > arrivalTotal) {
    pad.dataset.activity = "true";
    if (activityTimer !== null) window.clearTimeout(activityTimer);
    activityTimer = window.setTimeout(() => {
      pad.dataset.activity = "false";
      activityTimer = null;
    }, 180);
  }
  arrivalTotal = total;
  startButton.disabled = !canStartTrial(trial);
  stopButton.disabled = !canStopTrial(trial);
  returnButton.disabled = !canReturnToSetup(trial);
  returnButton.textContent = trial.closePending ? "Returning to setup…" : "Return to setup";
  schedulePoll();
}

function allowsControlKey(event) {
  if (!canNavigateTrialControls(trial)) return false;
  if (event.key === "Tab") return true;
  if (event.key !== "Enter" && event.key !== " ") return false;
  return event.target instanceof HTMLButtonElement && !event.target.disabled;
}

function phaseLabel(state) {
  if (state.closePending) return "Returning to setup";
  if (state.preflightFailure) return "Ready to retry";
  return (
    {
      prepared: "Ready",
      starting: `Waiting for ${state.peerName}`,
      active:
        state.sendsInput === true
          ? `Sending to ${state.peerName}`
          : state.sendsInput === false
            ? `Receiving from ${state.peerName}`
            : "Test running",
      stopping: "Stopping",
      finished: "Finished",
      error: "Needs attention",
    }[state.phase] || "Needs attention"
  );
}

function roleLabel(state) {
  const local =
    platform === "macos" ? "This Mac" : platform === "windows" ? "This PC" : "This computer";
  if (state.sendsInput === true)
    return `${local} is the input computer. It sends to ${state.peerName}.`;
  if (state.sendsInput === false)
    return `${local} receives input from ${state.peerName}. Input lands only inside this window.`;
  return "Bounded to this window. 60 seconds at most.";
}

function countLanded(kind) {
  if (trial.phase !== "active" || trial.sendsInput === true) return;
  landed[kind] += 1;
  landedNode.textContent = `Landed in this window · ${countSummary(landed)}`;
}

function countSummary(counts) {
  return `keys ${counts.keyboard} · clicks ${counts.button} · moves ${counts.movement} · scrolls ${counts.scroll}`;
}

function failureTitle(state) {
  if (state.closeError) return "Return to setup needs attention";
  if (state.preflightFailure) return "The test did not start";
  return "The test needs attention";
}

function nativeError(error) {
  return error instanceof Error && error.message ? error.message : String(error);
}
