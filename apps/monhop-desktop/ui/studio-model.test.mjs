import assert from "node:assert/strict";
import test from "node:test";

import {
  INSPECTOR_OVERLAY_BELOW,
  LOCAL,
  NETWORK_LOCKED,
  NEW,
  SETTINGS,
  arrangeState,
  canvasFingerprint,
  clipboardLine,
  computerSelection,
  defaultSelection,
  inspectorKind,
  inspectorPresentation,
  localRowStatus,
  networkChip,
  pairingRowLabel,
  resolveSelection,
  resumeTarget,
  sameSelection,
  selectionFromKey,
  selectionKey,
  sidebarRows,
  toolbarState,
} from "./studio-model.mjs";

const A = "a".repeat(64);
const B = "b".repeat(64);
const C = "c".repeat(64);

function computers(...fingerprints) {
  return {
    loaded: true,
    items: fingerprints.map((fingerprint, index) => ({
      fingerprint,
      name: `Computer ${index + 1}`,
      platform: index % 2 ? "macos" : "windows",
      address: "192.168.1.21:24872",
    })),
    active: null,
    enabled: [],
  };
}

test("each selection kind has one key, and a key reads back as the same selection", () => {
  for (const selection of [LOCAL, NEW, SETTINGS, computerSelection(A)])
    assert.ok(sameSelection(selectionFromKey(selectionKey(selection)), selection));
  assert.equal(selectionKey(computerSelection(A)), `computer:${A}`);
  assert.equal(selectionKey(null), "local");
  assert.equal(selectionKey({ kind: "page" }), "local");
  assert.equal(selectionFromKey("computer:"), null);
  assert.equal(selectionFromKey("home"), null);
  assert.equal(selectionFromKey(undefined), null);
});

test("with nothing chosen, a live pairing leads, then a fresh pairing, the computer in use, the first", () => {
  const fingerprints = [A, B, C];
  assert.equal(defaultSelection({ fingerprints, active: B, pairingLive: true }), NEW);
  assert.deepEqual(defaultSelection({ fingerprints, active: B, paired: C }), computerSelection(C));
  assert.deepEqual(defaultSelection({ fingerprints, active: B }), computerSelection(B));
  assert.deepEqual(defaultSelection({ fingerprints }), computerSelection(A));
  assert.deepEqual(
    defaultSelection({ fingerprints, active: "f".repeat(64) }),
    computerSelection(A),
  );
  assert.equal(defaultSelection({ fingerprints: [] }), LOCAL);
  assert.equal(defaultSelection({}), LOCAL);
});

test("the user's own pick sticks until that computer disappears", () => {
  const context = { fingerprints: [A, B], active: A };
  assert.deepEqual(
    resolveSelection({ ...context, chosen: computerSelection(B) }),
    computerSelection(B),
  );
  assert.equal(resolveSelection({ ...context, chosen: SETTINGS }), SETTINGS);
  assert.equal(resolveSelection({ ...context, chosen: LOCAL, pairingLive: true }), LOCAL);
  assert.deepEqual(
    resolveSelection({ fingerprints: [A], active: A, chosen: computerSelection(B) }),
    computerSelection(A),
  );
  assert.deepEqual(resolveSelection(context), computerSelection(A));
  assert.deepEqual(
    resolveSelection({ ...context, chosen: { kind: "home" } }),
    computerSelection(A),
  );
});

test("a computer a pairing just added keeps the pairing on screen until the list names it", () => {
  const chosen = computerSelection(C);
  assert.equal(resolveSelection({ fingerprints: [A], chosen, paired: C }), NEW);
  assert.deepEqual(resolveSelection({ fingerprints: [A, C], chosen, paired: C }), chosen);
});

test("the inspector follows the selection and steps aside for Settings", () => {
  assert.equal(inspectorKind(LOCAL), "local");
  assert.equal(inspectorKind(NEW), "new");
  assert.equal(inspectorKind(computerSelection(A)), "computer");
  assert.equal(inspectorKind(SETTINGS), null);
  assert.equal(inspectorKind(null), null);
});

test("below the overlay width the inspector becomes a sheet the user can close", () => {
  const wide = INSPECTOR_OVERLAY_BELOW;
  assert.deepEqual(inspectorPresentation({ width: wide, kind: "local" }), {
    mode: "docked",
    open: true,
  });
  assert.deepEqual(inspectorPresentation({ width: wide, kind: "local", dismissed: true }), {
    mode: "docked",
    open: true,
  });
  assert.deepEqual(inspectorPresentation({ width: wide - 1, kind: "computer" }), {
    mode: "overlay",
    open: true,
  });
  assert.deepEqual(inspectorPresentation({ width: 760, kind: "computer", dismissed: true }), {
    mode: "overlay",
    open: false,
  });
  assert.equal(inspectorPresentation({ width: 1180, kind: null }).open, false);
});

test("This Mac's row shows the session while one runs, otherwise how ready it is", () => {
  const sharing = { key: "sharing", label: "Sharing", tone: "active" };
  assert.deepEqual(
    localRowStatus({ verdict: { label: "Ready", tone: "done" }, session: sharing }),
    {
      label: "Sharing",
      tone: "active",
    },
  );
  assert.deepEqual(
    localRowStatus({
      verdict: { label: "Ready", tone: "done" },
      session: { key: "standby", label: "Paired", tone: "neutral" },
    }),
    { label: "Ready", tone: "connected" },
  );
  assert.deepEqual(localRowStatus({ verdict: { label: "Access needed", tone: "fail" } }), {
    label: "Access needed",
    tone: "checking",
  });
  assert.deepEqual(localRowStatus({}), { label: "Not checked yet", tone: "neutral" });
});

test("the sidebar lists This Mac, every paired computer with its own state, then the add row", () => {
  const list = computers(A, B);
  const rows = sidebarRows({
    computers: list,
    sharingView: { phase: "sharing", peerFingerprint: A, active: A },
    active: A,
    localName: "This Mac",
    localPlatform: "macos",
    localStatus: { label: "Sharing", tone: "active" },
    selection: computerSelection(B),
  });
  assert.deepEqual(
    rows.map((row) => row.key),
    ["local", `computer:${A}`, `computer:${B}`, "new"],
  );
  assert.equal(rows[1].label, "Sharing");
  assert.equal(rows[1].tone, "active");
  assert.equal(rows[2].label, "Paired");
  assert.equal(rows[2].selected, true);
  assert.equal(rows.filter((row) => row.selected).length, 1);
  assert.equal(rows[3].title, "Add a computer");
  assert.equal(rows[3].label, "Show or enter a code");
});

test("the add row becomes the selected New computer and says where the pairing is", () => {
  const base = {
    computers: computers(),
    localName: "This PC",
    localPlatform: "windows",
    localStatus: { label: "Ready", tone: "connected" },
    selection: NEW,
  };
  const showing = sidebarRows({ ...base, pairingStep: "show" }).at(-1);
  assert.equal(showing.title, "New computer");
  assert.equal(showing.label, "Waiting for the code");
  assert.equal(showing.tone, "checking");
  assert.equal(showing.selected, true);
  const verifying = sidebarRows({ ...base, pairingStep: "progress", pairingPhase: "verifying" });
  assert.equal(verifying.at(-1).label, "Checking the code");
  assert.equal(pairingRowLabel("progress", "unknown"), "Working");
  assert.equal(pairingRowLabel("nonsense"), "Show or enter a code");
  assert.equal(pairingRowLabel("paired"), "Paired");
});

test("arranging stays on while the link waits for Apply, and Done shows only when it can leave", () => {
  assert.deepEqual(arrangeState({}), { active: false, leavable: false });
  assert.deepEqual(arrangeState({ connected: true }), { active: true, leavable: false });
  assert.deepEqual(arrangeState({ connected: true, editing: true }), {
    active: true,
    leavable: true,
  });
  assert.deepEqual(arrangeState({ requested: true }), { active: true, leavable: true });

  const canvas = { selection: LOCAL, enabledCount: 0 };
  assert.equal(toolbarState({ ...canvas, arrange: arrangeState({}) }).arrange, null);
  assert.equal(
    toolbarState({ ...canvas, arrange: arrangeState({}), canArrange: true }).arrange,
    "begin",
  );
  assert.equal(
    toolbarState({ ...canvas, arrange: arrangeState({ connected: true }), canArrange: true })
      .arrange,
    null,
  );
  assert.equal(
    toolbarState({ ...canvas, arrange: arrangeState({ editing: true }), canArrange: true }).arrange,
    "done",
  );
});

test("the toolbar titles the view and offers Pause, Resume or neither", () => {
  const arrange = arrangeState({});
  const settings = toolbarState({ selection: SETTINGS, arrange, enabledCount: 1 });
  assert.equal(settings.title, "Settings");
  assert.equal(settings.canvasActions, false);
  const sharing = toolbarState({ selection: LOCAL, arrange, enabledCount: 2, resume: A });
  assert.equal(sharing.title, "Arrangement");
  assert.equal(sharing.pause, "pause");
  assert.equal(toolbarState({ selection: LOCAL, arrange, resume: A }).pause, "resume");
  assert.equal(toolbarState({ selection: LOCAL, arrange }).pause, null);
});

test("the canvas draws the selected computer, else the one in use, else the first", () => {
  const fingerprints = [A, B];
  assert.equal(canvasFingerprint({ selection: computerSelection(B), active: A, fingerprints }), B);
  assert.equal(canvasFingerprint({ selection: LOCAL, active: B, fingerprints }), B);
  assert.equal(canvasFingerprint({ selection: computerSelection(C), fingerprints }), A);
  assert.equal(canvasFingerprint({ selection: LOCAL, fingerprints: [] }), null);
});

test("Resume brings back the computer last in use, then the selected one, then the first", () => {
  const fingerprints = [A, B];
  assert.equal(resumeTarget({ lastActive: B, selection: computerSelection(A), fingerprints }), B);
  assert.equal(resumeTarget({ lastActive: C, selection: computerSelection(B), fingerprints }), B);
  assert.equal(resumeTarget({ lastActive: C, selection: LOCAL, fingerprints }), A);
  assert.equal(resumeTarget({ fingerprints: [] }), null);
});

test("the network chip names the connection and locks while a session runs", () => {
  const selected = { address: "192.168.1.14" };
  assert.deepEqual(networkChip({ selected, label: "Ethernet", eligibleCount: 2 }), {
    text: "Ethernet · 192.168.1.14",
    disabled: false,
    hint: "Both computers must be on this network.",
  });
  const locked = networkChip({
    selected,
    label: "Ethernet",
    eligibleCount: 2,
    sessionActive: true,
  });
  assert.equal(locked.disabled, true);
  assert.equal(locked.hint, NETWORK_LOCKED);
  assert.equal(networkChip({}).text, "Choose a network");
  assert.equal(networkChip({}).disabled, true);
  assert.equal(
    networkChip({ selected: { address: "" }, label: "Wi-Fi", eligibleCount: 1 }).text,
    "Wi-Fi",
  );
});

test("a computer's clipboard line says what the switch in Settings means for it", () => {
  const peers = [
    { fingerprint: A, peerEnabled: true },
    { fingerprint: B, peerEnabled: false },
  ];
  assert.equal(
    clipboardLine({ enabled: false, peers }, A, "Office PC"),
    "Clipboard sharing is off. Turn it on in Settings.",
  );
  assert.equal(
    clipboardLine({ enabled: true, peers }, A, "Office PC"),
    "Sharing the clipboard with Office PC.",
  );
  assert.equal(
    clipboardLine({ enabled: true, peers }, B, "Studio Mac"),
    "Turn on clipboard sharing on Studio Mac too.",
  );
  assert.equal(
    clipboardLine({ enabled: true, peers }, C, "Den PC"),
    "Copied text and images reach Den PC while you share input with it.",
  );
  assert.equal(
    clipboardLine(null, A, "Office PC"),
    "Clipboard sharing is off. Turn it on in Settings.",
  );
});
