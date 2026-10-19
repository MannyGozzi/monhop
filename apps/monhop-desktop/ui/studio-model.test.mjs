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
  isPaused,
  localRowStatus,
  networkChip,
  networkMenuItems,
  pairedOnScreen,
  pairingRowLabel,
  resolveSelection,
  resumeTarget,
  sameSelection,
  selectionFromKey,
  selectionKey,
  sidebarRows,
  listStep,
  toolbarState,
} from "./studio-model.mjs";
import { setupVerdict } from "./model.mjs";

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
  assert.deepEqual(localRowStatus({}), { label: "Not checked yet", tone: "neutral" });
});

// The row's dot and the inspector's verdict dot are one color: amber while waiting, red when it failed.
test("This Mac's row takes the setup verdict's tone", () => {
  const cases = [
    [{ label: "Ready", tone: "done" }, "connected"],
    [{ label: "Access needed", tone: "fail" }, "error"],
    [{ label: "Choose a network", tone: "wait" }, "checking"],
  ];
  for (const [verdict, tone] of cases)
    assert.deepEqual(localRowStatus({ verdict }), { label: verdict.label, tone });
  const setup = setupVerdict({
    nativeAvailable: true,
    snapshot: {
      platform: "macos",
      errors: [],
      permissions: { accessibility: false, inputMonitoring: true },
    },
  });
  assert.equal(setup.label, "Access needed");
  assert.equal(localRowStatus({ verdict: setup }).tone, "error");
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

test("arranging stays on while the link waits for Apply, and the toolbar's Arrange leaves once it starts", () => {
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
  // The Apply bar's own button is the one way out, so the toolbar offers nothing while arranging.
  for (const arrange of [arrangeState({ editing: true }), arrangeState({ requested: true })])
    assert.equal(toolbarState({ ...canvas, arrange, canArrange: true }).arrange, null);
});

test("the toolbar titles the view and offers Pause, Resume or neither", () => {
  const arrange = arrangeState({});
  const settings = toolbarState({ selection: SETTINGS, arrange, enabledCount: 1 });
  assert.equal(settings.title, "Settings");
  assert.equal(settings.canvasActions, false);
  const sharing = toolbarState({ selection: LOCAL, arrange, enabledCount: 2 });
  assert.equal(sharing.title, "Arrangement");
  assert.equal(sharing.pause, "pause");
  // A pause keeps the group switched on, so Resume shows exactly while that group is paused.
  assert.equal(
    toolbarState({ selection: LOCAL, arrange, enabledCount: 1, paused: true }).pause,
    "resume",
  );
  // Nothing switched on: no group to pause or resume; the computer's own switch is the way in.
  assert.equal(toolbarState({ selection: LOCAL, arrange }).pause, null);
  assert.equal(toolbarState({ selection: LOCAL, arrange, paused: true }).pause, null);
});

test("the live view says whether sharing is paused, and the computer list until it answers", () => {
  assert.equal(isPaused({ sharingView: { paused: true }, computers: { paused: false } }), true);
  assert.equal(isPaused({ sharingView: { paused: false }, computers: { paused: true } }), false);
  assert.equal(isPaused({ sharingView: null, computers: { paused: true } }), true);
  assert.equal(isPaused({}), false);
});

test("the canvas draws the selected computer, else the one in use, else the first", () => {
  const fingerprints = [A, B];
  assert.equal(canvasFingerprint({ selection: computerSelection(B), active: A, fingerprints }), B);
  assert.equal(canvasFingerprint({ selection: LOCAL, active: B, fingerprints }), B);
  assert.equal(canvasFingerprint({ selection: computerSelection(C), fingerprints }), A);
  assert.equal(canvasFingerprint({ selection: LOCAL, fingerprints: [] }), null);
});

test("Resume brings back the paused group: the computer last in use when it is in it, else its first", () => {
  assert.equal(resumeTarget({ lastActive: B, enabled: [A, B] }), B);
  assert.equal(resumeTarget({ lastActive: C, enabled: [A, B] }), A);
  assert.equal(resumeTarget({ enabled: [B] }), B);
  // Never a computer outside the group, even the one last in use.
  assert.equal(resumeTarget({ lastActive: C, enabled: [] }), null);
  assert.equal(resumeTarget({}), null);
});

test("the network chip names the connection and locks while a session runs", () => {
  const selected = { address: "192.168.1.14" };
  assert.deepEqual(networkChip({ selected, label: "Ethernet", eligibleCount: 2 }), {
    name: "Ethernet",
    address: "192.168.1.14",
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

const PAIRED_VIEW = {
  peerFingerprint: C,
  peerPlatform: "macos",
  badge: { color: 2, symbols: [1, 2, 3] },
};

test("a finished pairing on screen is the computer it added, and only on the New computer row", () => {
  assert.deepEqual(pairedOnScreen({ selection: NEW, pairingStep: "paired", view: PAIRED_VIEW }), {
    fingerprint: C,
    platform: "macos",
    badge: PAIRED_VIEW.badge,
  });
  assert.equal(pairedOnScreen({ selection: NEW, pairingStep: "show", view: PAIRED_VIEW }), null);
  assert.equal(
    pairedOnScreen({ selection: computerSelection(C), pairingStep: "paired", view: PAIRED_VIEW }),
    null,
  );
  assert.equal(pairedOnScreen({ selection: NEW, pairingStep: "paired", view: null }), null);
});

test("the pairing row becomes the computer it added, listed once, until Done or Pair another", () => {
  const list = computers(A, C);
  const base = {
    computers: list,
    localName: "This Mac",
    localPlatform: "macos",
    localStatus: { label: "Ready", tone: "connected" },
    pairingStep: "paired",
  };
  const paired = pairedOnScreen({ selection: NEW, pairingStep: "paired", view: PAIRED_VIEW });
  const rows = sidebarRows({ ...base, selection: NEW, paired });
  assert.deepEqual(
    rows.map((row) => row.key),
    ["local", `computer:${A}`, `computer:${C}`],
  );
  const row = rows.at(-1);
  assert.equal(row.title, "Computer 2");
  assert.equal(row.label, "Paired");
  // The same quiet dot as every other paired computer's.
  assert.equal(row.tone, "neutral");
  assert.equal(row.platform, "macos");
  assert.equal(row.select, "new");
  assert.equal(row.adopt, "new");
  assert.equal(row.badgeShown, true);
  assert.deepEqual(row.badge, PAIRED_VIEW.badge);
  assert.equal(rows.filter((item) => item.selected).length, 1);
  assert.equal(row.selected, true);

  // Before the list names it, the row still reads as that computer, by platform.
  const early = sidebarRows({ ...base, computers: computers(A), selection: NEW, paired });
  assert.equal(early.at(-1).title, "Mac");
  assert.equal(early.at(-1).key, `computer:${C}`);

  // Done selects the computer: its row keeps its key, now selecting itself, and the add row returns.
  const done = sidebarRows({ ...base, selection: computerSelection(C) });
  assert.deepEqual(
    done.map((item) => item.key),
    ["local", `computer:${A}`, `computer:${C}`, "new"],
  );
  assert.equal(done[2].select, `computer:${C}`);
  assert.equal(done[2].selected, true);
  assert.equal(done[2].adopt, undefined);
  assert.equal(done[3].title, "Add a computer");

  // Pair another goes back to choosing: the computer is a plain row and the exchange is selected.
  const again = sidebarRows({ ...base, selection: NEW, pairingStep: "choose" });
  assert.deepEqual(
    again.map((item) => item.key),
    ["local", `computer:${A}`, `computer:${C}`, "new"],
  );
  assert.equal(again.at(-1).title, "New computer");
  assert.equal(again.at(-1).selected, true);
});

test("arrow keys, Home and End step through a list and hold at the ends", () => {
  assert.equal(listStep(4, 1, "ArrowDown"), 2);
  assert.equal(listStep(4, 3, "ArrowDown"), 3);
  assert.equal(listStep(4, 0, "ArrowUp"), 0);
  assert.equal(listStep(4, 2, "ArrowUp"), 1);
  assert.equal(listStep(4, 2, "Home"), 0);
  assert.equal(listStep(4, 0, "End"), 3);
  assert.equal(listStep(4, -1, "ArrowDown"), 0);
  assert.equal(listStep(4, -1, "ArrowUp"), 3);
  assert.equal(listStep(4, 1, "Enter"), -1);
  assert.equal(listStep(0, 0, "ArrowDown"), -1);
});

test("the network menu checks the chosen network and greys out the ones MonHop cannot use", () => {
  const interfaces = [
    {
      id: "en0",
      kind: "Wi-Fi",
      networkName: "Gozzi Home",
      name: "Wi-Fi",
      address: "192.168.1.20",
      physical: true,
      up: true,
      attachmentKnown: true,
    },
    { id: "en1", kind: "ethernet", name: "Ethernet", address: "", physical: true, up: true },
    { id: "utun3", kind: "VPN", name: "VPN", address: "10.8.0.2", physical: false, up: true },
    {
      id: "en2",
      kind: "ethernet",
      name: "Thunderbolt Bridge",
      address: "",
      physical: true,
      up: false,
    },
  ];
  assert.deepEqual(networkMenuItems(interfaces, "en0"), [
    { id: "en0", label: "Gozzi Home", detail: "192.168.1.20", checked: true, disabled: false },
    {
      id: "en1",
      label: "Ethernet",
      detail: "Ethernet · Not recognized",
      checked: false,
      disabled: false,
    },
    { id: "utun3", label: "VPN", detail: "Not a hardware network", checked: false, disabled: true },
    {
      id: "en2",
      label: "Thunderbolt Bridge",
      detail: "Not connected",
      checked: false,
      disabled: true,
    },
  ]);
  assert.deepEqual(networkMenuItems(), []);
});
