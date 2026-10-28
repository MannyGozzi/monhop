import assert from "node:assert/strict";
import test from "node:test";

import { normalizeComputers } from "./computers-model.mjs";
import {
  MAX_STEP,
  MIN_STEP,
  POINTER_SPEED_DESCRIPTION,
  POINTER_SPEED_ENDS,
  POINTER_SPEED_LABEL,
  POINTER_SPEED_NOT_SAVED,
  SAME,
  applyPointerSpeedStatus,
  choosePointerSpeed,
  failPointerSpeed,
  initialPointerSpeed,
  normalizePointerSpeedView,
  pointerPageDescription,
  pointerPageTitle,
  pointerPeerRows,
  pointerSliderTitle,
  pointerSpeedContext,
  pointerSpeedText,
  pointerSpeedValueText,
  settlePointerSpeed,
} from "./pointer-speed-model.mjs";

const PC = "b".repeat(64);
const MAC = "c".repeat(64);
const OTHER = "d".repeat(64);

function computersOf(...entries) {
  return normalizeComputers({
    computers: entries.map(([fingerprint, platform, name]) => ({ fingerprint, platform, name })),
    enabled: entries.map(([fingerprint]) => fingerprint),
  });
}

test("the range and Same match the Rust contract", () => {
  assert.deepEqual([MIN_STEP, SAME, MAX_STEP], [-5, 0, 5]);
});

test("the readout says Same or a whole percent, from half to twice the speed", () => {
  assert.equal(pointerSpeedText(SAME), "Same");
  assert.equal(pointerSpeedText(2), "132%");
  assert.equal(pointerSpeedText(-2), "76%");
  assert.equal(pointerSpeedText(MAX_STEP), "200%");
  assert.equal(pointerSpeedText(MIN_STEP), "50%");
});

test("the spoken value says how fast and which way", () => {
  assert.equal(pointerSpeedValueText(SAME), "Same");
  assert.equal(pointerSpeedValueText(2), "132 percent, faster");
  assert.equal(pointerSpeedValueText(-2), "76 percent, slower");
});

test("each platform names its own page and slider", () => {
  assert.equal(pointerPageTitle("macos"), "Mouse and trackpad");
  assert.equal(pointerPageTitle("windows"), "Mouse");
  assert.equal(pointerSliderTitle("macos"), "This Mac's mouse and trackpad");
  assert.equal(pointerSliderTitle("windows"), "This PC's mouse");
  assert.match(pointerPageDescription("macos"), /this Mac/);
  assert.match(pointerPageDescription("windows"), /this PC/);
  assert.equal(
    POINTER_SPEED_DESCRIPTION,
    "Same moves the pointer on other computers the way it moves here.",
  );
  assert.equal(POINTER_SPEED_LABEL, "Pointer speed on other computers");
  assert.deepEqual(POINTER_SPEED_ENDS, ["Slower", "Faster"]);
});

test("a malformed or out-of-range view reads as Same", () => {
  assert.deepEqual(normalizePointerSpeedView(undefined), { step: SAME, error: "" });
  assert.deepEqual(normalizePointerSpeedView({ step: 3, error: null }), { step: 3, error: "" });
  for (const step of [6, -6, 1.5, "2", null])
    assert.equal(normalizePointerSpeedView({ step }).step, SAME);
  assert.equal(normalizePointerSpeedView({ step: 1, error: "unread" }).error, "unread");
});

test("the page shows on a Mac or a PC only, at Same until Rust answers", () => {
  assert.equal(pointerSpeedContext("other", initialPointerSpeed()), undefined);
  assert.deepEqual(pointerSpeedContext("macos", initialPointerSpeed()), {
    step: SAME,
    message: "",
  });
  const loaded = applyPointerSpeedStatus(initialPointerSpeed(), {
    step: -1,
    error: "could not be read",
  });
  assert.deepEqual(pointerSpeedContext("windows", loaded), {
    step: -1,
    message: "could not be read",
  });
});

test("the newest choice shows at once and only its own answer settles it", () => {
  let state = applyPointerSpeedStatus(initialPointerSpeed(), { step: 0, error: null });
  state = choosePointerSpeed(state, 2);
  state = choosePointerSpeed(state, 4);
  assert.equal(pointerSpeedContext("macos", state).step, 4);
  state = settlePointerSpeed(state, 2, { step: 2, error: null });
  assert.equal(state.wanted, 4);
  assert.equal(pointerSpeedContext("macos", state).step, 4);
  state = settlePointerSpeed(state, 4, { step: 4, error: null });
  assert.deepEqual(state, { view: { step: 4, error: null }, wanted: null, failed: false });
});

test("a status answer never replaces a view a saved choice already gave", () => {
  const saved = settlePointerSpeed(choosePointerSpeed(initialPointerSpeed(), 3), 3, { step: 3 });
  assert.equal(applyPointerSpeedStatus(saved, { step: 0 }), saved);
});

test("a failed save shows the not-saved note until a choice saves", () => {
  let state = choosePointerSpeed(initialPointerSpeed(), 1);
  state = settlePointerSpeed(state, 1, { step: 1, error: "could not be saved" });
  assert.deepEqual(pointerSpeedContext("macos", state), {
    step: 1,
    message: POINTER_SPEED_NOT_SAVED,
  });
  state = failPointerSpeed(choosePointerSpeed(state, 2), 2);
  assert.equal(pointerSpeedContext("macos", state).step, 1);
  assert.equal(pointerSpeedContext("macos", state).message, POINTER_SPEED_NOT_SAVED);
  assert.equal(failPointerSpeed(choosePointerSpeed(state, 3), 2).wanted, 3);
  state = settlePointerSpeed(choosePointerSpeed(state, 3), 3, { step: 3, error: null });
  assert.equal(pointerSpeedContext("macos", state).message, "");
});

test("each computer that can control this one points to its own page", () => {
  const computers = computersOf([PC, "windows", "Studio PC"], [MAC, "macos", "Laptop"]);
  assert.deepEqual(pointerPeerRows({ computers, sharingView: null, active: PC }), [
    {
      key: PC,
      name: "Studio PC",
      note: "Change how fast its pointer moves here in MonHop on Studio PC, under Settings › Mouse.",
    },
    {
      key: MAC,
      name: "Laptop",
      note: "Change how fast its pointer moves here in MonHop on Laptop, under Settings › Mouse and trackpad.",
    },
  ]);
});

test("a computer switched out or barred from controlling this one has no row", () => {
  const computers = normalizeComputers({
    computers: [
      { fingerprint: PC, platform: "windows", name: "" },
      { fingerprint: MAC, platform: "macos", name: "Laptop" },
      { fingerprint: OTHER, platform: "windows", name: "Spare" },
    ],
    enabled: [PC, MAC],
  });
  const sharingView = {
    control: {
      localToPeer: true,
      peerToLocal: true,
      members: [
        { fingerprint: PC, allowed: true },
        { fingerprint: MAC, allowed: false },
      ],
    },
  };
  assert.deepEqual(
    pointerPeerRows({ computers, sharingView, active: PC }).map((row) => [row.key, row.name]),
    [[PC, "Windows PC"]],
  );
});
