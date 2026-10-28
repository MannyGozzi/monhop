// Pointer speed as Settings shows it: this computer's own speed on the computers it controls, saved
// by Rust, and a row for each computer whose pointer moves here, naming where that one's speed is set.

import { memberPeerToLocal, resolveEnabledList } from "./computer-card-model.mjs";
import { displayName } from "./computers-model.mjs";

// As monhop_core::pointer_speed defines them: whole steps either side of Same, a fifth of a doubling each.
export const MIN_STEP = -5;
export const MAX_STEP = 5;
export const SAME = 0;
const STEPS_PER_DOUBLING = 5;

export const POINTER_SPEED_TITLE = "Pointer speed";
export const POINTER_SPEED_DESCRIPTION =
  "Same moves the pointer on other computers the way it moves here.";
export const POINTER_SPEED_LABEL = "Pointer speed on other computers";
export const POINTER_SPEED_ENDS = ["Slower", "Faster"];
export const POINTER_SPEED_NOT_SAVED = "The pointer speed was not saved. Try again.";

// The Settings page that holds a computer's own pointer speed, as that computer's rail names it.
export function pointerPageTitle(platform) {
  return platform === "macos" ? "Mouse and trackpad" : "Mouse";
}

export function pointerPageDescription(platform) {
  return platform === "macos"
    ? "When this Mac's mouse or trackpad controls another computer, or another computer's mouse controls this Mac."
    : "When this PC's mouse controls another computer, or another computer's mouse controls this PC.";
}

export function pointerSliderTitle(platform) {
  return platform === "macos" ? "This Mac's mouse and trackpad" : "This PC's mouse";
}

function percent(step) {
  return Math.round(2 ** (step / STEPS_PER_DOUBLING) * 100);
}

export function pointerSpeedText(step) {
  return step === SAME ? "Same" : `${percent(step)}%`;
}

export function pointerSpeedValueText(step) {
  if (step === SAME) return "Same";
  return `${percent(step)} percent, ${step > SAME ? "faster" : "slower"}`;
}

export function normalizePointerSpeedView(value) {
  const source = value && typeof value === "object" ? value : {};
  const { step } = source;
  return {
    step: Number.isInteger(step) && step >= MIN_STEP && step <= MAX_STEP ? step : SAME,
    error: typeof source.error === "string" ? source.error : "",
  };
}

// Rust's view (undefined until its status answers), the step chosen here that Rust has not answered
// for yet, and whether the newest choice failed to save.
export function initialPointerSpeed() {
  return { view: undefined, wanted: null, failed: false };
}

// The status answer only fills in a view no saved choice has replaced yet.
export function applyPointerSpeedStatus(state, view) {
  return state.view === undefined ? { ...state, view } : state;
}

export function choosePointerSpeed(state, step) {
  return { ...state, wanted: step };
}

// Rust's answer to the choice `sent`. Only an answer to the newest choice settles what is shown.
export function settlePointerSpeed(state, sent, view) {
  if (state.wanted !== sent) return { ...state, view };
  return { view, wanted: null, failed: normalizePointerSpeedView(view).error !== "" };
}

export function failPointerSpeed(state, sent) {
  return state.wanted === sent ? { ...state, wanted: null, failed: true } : state;
}

// The page's context: a Mac or a PC only, at Same until Rust answers.
export function pointerSpeedContext(platform, state) {
  if (platform !== "macos" && platform !== "windows") return undefined;
  const view = normalizePointerSpeedView(state.view);
  return {
    step: state.wanted ?? view.step,
    message: state.failed ? POINTER_SPEED_NOT_SAVED : view.error,
  };
}

// One row per computer switched in whose pointer may move here, saying where its own speed is set.
export function pointerPeerRows({ computers, sharingView, active }) {
  const enabled = resolveEnabledList(sharingView, computers, active);
  return computers.items
    .filter(
      (computer) =>
        enabled.includes(computer.fingerprint) &&
        memberPeerToLocal(sharingView?.control, computer.fingerprint),
    )
    .map((computer) => {
      const name = displayName(computer);
      return {
        key: computer.fingerprint,
        name,
        note: `Change how fast its pointer moves here in MonHop on ${name}, under Settings › ${pointerPageTitle(computer.platform)}.`,
      };
    });
}
