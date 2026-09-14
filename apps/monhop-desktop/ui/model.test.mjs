import assert from "node:assert/strict";
import test from "node:test";

import {
  accessReady,
  applySnapshot,
  autoSelectInterface,
  networkReady,
  canSelectInterface,
  errorMessages,
  hasRelevantWifiInterface,
  initialState,
  interfaceAssessment,
  interfaceAuthorizationGate,
  nativeCheckState,
  networkDetail,
  networkLabel,
  normalizeSnapshot,
  pairingOpenGate,
  permissionRows,
  selectInterface,
  setupVerdict,
  wifiAuthorizationStatus,
  wifiRecognition,
} from "./model.mjs";

const macSnapshot = {
  platform: "macos",
  version: "0.1.0",
  permissions: { accessibility: true, inputMonitoring: false },
  launch: { executable: "/Applications/MonHop.app", bundled: true },
  interfaces: [
    {
      id: "en0",
      index: 4,
      name: "Ethernet",
      address: "192.168.50.10",
      prefixLength: 24,
      kind: "ethernet",
      physical: true,
      up: true,
      attachmentKnown: false,
    },
    {
      id: "utun2",
      index: 9,
      name: "VPN",
      address: "10.0.0.2",
      prefixLength: 24,
      kind: "vpn",
      physical: false,
      up: true,
      attachmentKnown: true,
    },
  ],
  displays: [],
  errors: [],
  pairingAvailable: false,
};

test("the log file path survives snapshot normalization only as a string", () => {
  const path = "/Users/me/Library/Application Support/com.manuelgozzi.monhop/logs/monhop.log";
  assert.equal(normalizeSnapshot({ ...macSnapshot, logPath: path }).logPath, path);
  assert.equal(normalizeSnapshot({ ...macSnapshot, logPath: 7 }).logPath, "");
  assert.equal(normalizeSnapshot(macSnapshot).logPath, "");
});

test("native check state stays distinct for startup, checked data, and browser preview", () => {
  const unchecked = initialState(true);
  assert.equal(nativeCheckState(unchecked), "unchecked");
  assert.equal(nativeCheckState(applySnapshot(unchecked, macSnapshot)), "checked");
  assert.equal(nativeCheckState(initialState(false)), "preview");
});

test("macOS permission rows keep allowed, denied, and unknown distinct", () => {
  const state = applySnapshot(initialState(true), macSnapshot);
  const rows = permissionRows(state.snapshot);
  assert.deepEqual(
    rows.map((row) => [row.key, row.status.label]),
    [
      ["accessibility", "Allowed"],
      ["inputMonitoring", "Not allowed"],
    ],
  );
  assert.equal(rows[0].recovery, null);
  assert.deepEqual(rows[1].recovery, {
    summary: "Fix Input Monitoring in Settings",
    pane: "System Settings > Privacy & Security > Input Monitoring",
    restartNote: "Input Monitoring can require quitting and reopening MonHop.",
  });

  const bothAllowed = applySnapshot(initialState(true), {
    ...macSnapshot,
    permissions: { accessibility: true, inputMonitoring: true },
  });
  assert.equal(accessReady(bothAllowed.snapshot), true);
  assert.equal(setupVerdict(bothAllowed).label, "Choose a network");

  const bothDenied = applySnapshot(initialState(true), {
    ...macSnapshot,
    permissions: { accessibility: false, inputMonitoring: false },
  });
  assert.deepEqual(permissionRows(bothDenied.snapshot)[0].recovery, {
    summary: "Fix Accessibility in Settings",
    pane: "System Settings > Privacy & Security > Accessibility",
    restartNote: null,
  });
  assert.equal(accessReady(bothDenied.snapshot), false);
  assert.equal(setupVerdict(bothDenied).label, "Access needed");

  const unknown = applySnapshot(initialState(true), {
    ...macSnapshot,
    permissions: { accessibility: null, inputMonitoring: null },
  });
  assert.deepEqual(
    permissionRows(unknown.snapshot).map((row) => row.status.label),
    ["Unknown", "Unknown"],
  );
  assert.deepEqual(
    permissionRows(unknown.snapshot).map((row) => row.recovery),
    [null, null],
  );
  assert.equal(accessReady(unknown.snapshot), false);
});

test("Windows may move to network without showing a macOS permission result", () => {
  const windows = applySnapshot(initialState(true), { ...macSnapshot, platform: "windows" });
  assert.deepEqual(permissionRows(windows.snapshot), []);
  assert.equal(accessReady(windows.snapshot), true);
});

test("Wi-Fi authorization accepts only bounded categories and keeps permission separate from recognition", () => {
  const statuses = [
    ["not-determined", "Not asked yet"],
    ["denied", "Not allowed"],
    ["restricted", "Restricted"],
    ["authorized", "Allowed"],
    ["services-disabled", "Location Services off"],
    ["unknown", "Unknown"],
  ];
  for (const [value, label] of statuses) {
    const snapshot = applySnapshot(initialState(true), {
      ...macSnapshot,
      permissions: { ...macSnapshot.permissions, wifiAuthorization: value },
    }).snapshot;
    assert.equal(snapshot.permissions.wifiAuthorization, value);
    assert.equal(wifiAuthorizationStatus(snapshot.permissions.wifiAuthorization).label, label);
  }

  for (const value of [undefined, null, "granted", "authorized ", 1, {}]) {
    const snapshot = applySnapshot(initialState(true), {
      ...macSnapshot,
      permissions: { ...macSnapshot.permissions, wifiAuthorization: value },
    }).snapshot;
    assert.equal(snapshot.permissions.wifiAuthorization, "unknown");
  }

  const wifiUnknown = applySnapshot(initialState(true), {
    ...macSnapshot,
    permissions: { ...macSnapshot.permissions, wifiAuthorization: "authorized" },
    interfaces: [{ ...macSnapshot.interfaces[0], kind: "Wi-Fi" }],
  }).snapshot;
  assert.equal(hasRelevantWifiInterface(wifiUnknown), true);
  assert.deepEqual(wifiRecognition(wifiUnknown), {
    label: "Not recognized",
    detail:
      "Access is allowed, but the network is still unknown. Check again. If it stays unknown, this build cannot use it yet.",
  });

  const wifiRecognized = {
    ...wifiUnknown,
    interfaces: [{ ...wifiUnknown.interfaces[0], attachmentKnown: true }],
  };
  assert.equal(wifiRecognition(wifiRecognized).label, "Recognized");
  assert.match(wifiRecognition(wifiRecognized).detail, /identify this network/);
  assert.equal(hasRelevantWifiInterface({ ...wifiUnknown, platform: "windows" }), false);
});

test("network selection stays local and unknown recognition blocks pairing", () => {
  let state = applySnapshot(initialState(true), {
    ...macSnapshot,
    permissions: { ...macSnapshot.permissions, wifiAuthorization: "authorized" },
  });
  assert.equal(canSelectInterface(state.snapshot.interfaces[0]), true);
  assert.equal(canSelectInterface(state.snapshot.interfaces[1]), false);

  state = selectInterface(state, "utun2");
  assert.equal(state.selectedInterfaceId, null);
  state = selectInterface(state, "en0");
  assert.equal(state.selectedInterfaceId, "en0");
  assert.equal(interfaceAssessment(state.snapshot.interfaces[0]), "Network not recognized");
  assert.deepEqual(interfaceAuthorizationGate(state.snapshot.interfaces[0]), {
    label: "Not recognized",
    tone: "needed",
    detail: "MonHop cannot confirm this network yet. Pairing stays off.",
  });
  assert.equal(networkReady(state), false);
  const recognized = applySnapshot(state, {
    ...macSnapshot,
    interfaces: [{ ...macSnapshot.interfaces[0], attachmentKnown: true }],
  });
  assert.equal(networkReady(selectInterface(recognized, "en0")), true);
});

test("a single recognized network is chosen automatically and several stay a choice", () => {
  const one = applySnapshot(initialState(true), {
    ...macSnapshot,
    interfaces: [
      { ...macSnapshot.interfaces[0], attachmentKnown: true },
      macSnapshot.interfaces[1],
    ],
  });
  assert.equal(autoSelectInterface(one).selectedInterfaceId, "en0");
  const two = applySnapshot(initialState(true), {
    ...macSnapshot,
    interfaces: [
      { ...macSnapshot.interfaces[0], attachmentKnown: true },
      {
        ...macSnapshot.interfaces[0],
        id: "en1",
        name: "Wi-Fi",
        kind: "Wi-Fi",
        attachmentKnown: true,
      },
    ],
  });
  assert.equal(autoSelectInterface(two).selectedInterfaceId, null);
  assert.equal(
    autoSelectInterface(applySnapshot(initialState(true), macSnapshot)).selectedInterfaceId,
    null,
  );
  // The network chosen last time wins over the automatic choice, and a vanished one decides nothing.
  assert.equal(autoSelectInterface(two, "en1").selectedInterfaceId, "en1");
  assert.equal(autoSelectInterface(two, "en9").selectedInterfaceId, null);
  assert.equal(autoSelectInterface(selectInterface(two, "en0"), "en1").selectedInterfaceId, "en0");
});

test("snapshot replacement revokes a vanished network choice", () => {
  let state = applySnapshot(initialState(true), macSnapshot);
  state = selectInterface(state, "en0");
  state = applySnapshot(state, { ...macSnapshot, interfaces: [] });
  assert.equal(state.selectedInterfaceId, null);
});

test("pairing capability permits setup but reported sharing remains unusable", () => {
  const pairingState = selectInterface(
    applySnapshot(initialState(true), {
      ...macSnapshot,
      pairingAvailable: true,
      interfaces: [{ ...macSnapshot.interfaces[0], attachmentKnown: true }],
    }),
    "en0",
  );
  assert.deepEqual(pairingOpenGate(pairingState), {
    allowed: true,
    detail: "Reads this computer's saved identity. macOS may ask to unlock it.",
  });
});

test("error messages remain text and preview stays outside setup", () => {
  const state = {
    ...applySnapshot(initialState(true), {
      ...macSnapshot,
      errors: ["Native route check remains unavailable"],
    }),
    messages: ["<script>not executable</script>"],
  };
  assert.deepEqual(errorMessages(state), [
    "Native route check remains unavailable",
    "<script>not executable</script>",
  ]);
  assert.equal(setupVerdict(initialState(false)).label, "Preview only");
  assert.equal(
    setupVerdict(initialState(true), { checking: true }).label,
    "Checking this computer",
  );
  assert.equal(setupVerdict(initialState(true)).label, "Not checked yet");
});

test("failed checks and partial reports do not present a successful next step", () => {
  const snapshot = { ...macSnapshot, permissions: { accessibility: true, inputMonitoring: true } };
  const failed = { ...applySnapshot(initialState(true), snapshot), actionFailed: true };
  assert.equal(setupVerdict(failed).label, "Something did not finish");
  const partial = applySnapshot(initialState(true), {
    ...snapshot,
    errors: ["Could not read networks"],
  });
  assert.equal(setupVerdict(partial).label, "A check needs attention");
  assert.equal(accessReady(partial.snapshot), false);
  assert.equal(accessReady({ ...partial.snapshot, errors: [] }), true);
});

test("the verdict says ready only with access and a recognized network", () => {
  const ready = selectInterface(
    applySnapshot(initialState(true), {
      ...macSnapshot,
      permissions: { accessibility: true, inputMonitoring: true },
      interfaces: [{ ...macSnapshot.interfaces[0], attachmentKnown: true }],
    }),
    "en0",
  );
  assert.equal(setupVerdict(ready).label, "Ready");
  assert.match(setupVerdict(ready).detail, /Ethernet/);
  assert.equal(setupVerdict(ready).tone, "done");
});

test("pairing stays inert until a checked capability and recognized selection exist", () => {
  assert.equal(pairingOpenGate(initialState(true)).allowed, false);
  const unrecognized = selectInterface(
    applySnapshot(initialState(true), {
      ...macSnapshot,
      pairingAvailable: true,
    }),
    "en0",
  );
  assert.equal(pairingOpenGate(unrecognized).allowed, false);
});

test("network labels prefer a Wi-Fi name and fall back to the adapter without changing detail", () => {
  const wifi = {
    id: "wifi-1",
    name: "en0",
    networkName: "Studio <b>North</b>",
    kind: "Wi-Fi",
    physical: true,
    up: true,
    attachmentKnown: true,
  };
  assert.equal(networkLabel(wifi), "Studio <b>North</b>");
  assert.equal(networkDetail(wifi), "Wi-Fi · Network recognized");
  assert.equal(networkLabel({ ...wifi, networkName: "" }), "Wi-Fi");
  assert.equal(
    networkDetail({ ...wifi, networkName: "" }),
    "Wi-Fi · Network recognized · Name unavailable",
  );

  const ethernet = {
    ...wifi,
    kind: "Ethernet",
    name: "USB <em>Ethernet</em>",
    networkName: "Ignored Wi-Fi name",
  };
  assert.equal(networkLabel(ethernet), "USB <em>Ethernet</em>");
  assert.equal(networkDetail(ethernet), "Ethernet · Network recognized");
  assert.equal(networkLabel({ ...ethernet, name: "" }), "Ethernet");
});

test("snapshot network labels are byte-bounded display text without changing selection or pairing gates", () => {
  const maxAscii = "a".repeat(32);
  const maxUtf8 = "é".repeat(16);
  for (const value of [maxAscii, maxUtf8, "<strong>Studio</strong>"]) {
    const snapshot = normalizeSnapshot({
      interfaces: [{ id: "wifi-1", name: "en0", networkName: value }],
    });
    assert.equal(snapshot.interfaces[0].networkName, value);
  }

  for (const value of ["a".repeat(33), "é".repeat(17), "  \t", "home\nother", "home\u202eother"]) {
    const snapshot = normalizeSnapshot({
      interfaces: [{ id: "wifi-1", name: "en0", networkName: value }],
    });
    assert.equal(snapshot.interfaces[0].networkName, "");
  }

  for (const value of ["<strong>en0</strong>", "a".repeat(256)]) {
    const snapshot = normalizeSnapshot({
      interfaces: [{ id: "wifi-1", name: value, networkName: "Studio" }],
    });
    assert.equal(snapshot.interfaces[0].name, value);
  }
  for (const value of ["a".repeat(257), "  \t", "en0\nother", "en0\u200eother"]) {
    const snapshot = normalizeSnapshot({
      interfaces: [{ id: "wifi-1", name: value, networkName: "Studio" }],
    });
    assert.equal(snapshot.interfaces[0].name, "");
  }

  let state = applySnapshot(initialState(true), {
    ...macSnapshot,
    pairingAvailable: true,
    interfaces: [
      {
        id: "wifi-1",
        index: 4,
        name: "\u202ehidden",
        networkName: "\u0000hidden",
        address: "192.168.50.10",
        prefixLength: 24,
        kind: "Wi-Fi",
        physical: true,
        up: true,
        attachmentKnown: true,
      },
    ],
  });
  const item = state.snapshot.interfaces[0];
  assert.equal(item.name, "");
  assert.equal(item.networkName, "");
  assert.equal(item.id, "wifi-1");
  assert.equal(item.attachmentKnown, true);
  assert.equal(canSelectInterface(item), true);
  assert.equal(networkLabel(item), "Wi-Fi");
  assert.equal(networkDetail(item), "Wi-Fi · Network recognized · Name unavailable");

  state = selectInterface(state, item.id);
  assert.equal(state.selectedInterfaceId, item.id);
  assert.equal(pairingOpenGate(state).allowed, true);
});
