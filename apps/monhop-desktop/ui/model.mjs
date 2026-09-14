const PLATFORMS = new Set(["macos", "windows", "unsupported"]);
const WIFI_AUTHORIZATION = new Set([
  "not-determined",
  "denied",
  "restricted",
  "authorized",
  "services-disabled",
  "unknown",
]);

export function initialState(nativeAvailable) {
  return {
    busy: false,
    actionFailed: false,
    messages: [],
    nativeAvailable,
    selectedInterfaceId: null,
    snapshot: null,
  };
}

export function applySnapshot(state, value) {
  const snapshot = normalizeSnapshot(value);
  const selected = snapshot.interfaces.find((item) => item.id === state.selectedInterfaceId);
  return {
    ...state,
    snapshot,
    selectedInterfaceId: selected && canSelectInterface(selected) ? selected.id : null,
  };
}

export function normalizeSnapshot(value) {
  const source = value && typeof value === "object" ? value : {};
  const permissions =
    source.permissions && typeof source.permissions === "object" ? source.permissions : {};
  const launch = source.launch && typeof source.launch === "object" ? source.launch : {};
  return {
    platform: PLATFORMS.has(source.platform) ? source.platform : "unsupported",
    version: text(source.version),
    logPath: text(source.logPath),
    permissions: {
      accessibility: booleanOrNull(permissions.accessibility),
      inputMonitoring: booleanOrNull(permissions.inputMonitoring),
      wifiAuthorization: wifiAuthorization(permissions.wifiAuthorization),
    },
    launch: {
      executable: text(launch.executable),
      bundled: launch.bundled === true,
    },
    interfaces: array(source.interfaces).map(normalizeInterface),
    displays: array(source.displays).map(normalizeDisplay),
    errors: array(source.errors).map(text).filter(Boolean),
    pairingAvailable: source.pairingAvailable === true,
  };
}

export function nativeCheckState(state) {
  if (!state.nativeAvailable) return "preview";
  return state.snapshot ? "checked" : "unchecked";
}

export function permissionRows(snapshot) {
  if (!snapshot || snapshot.platform !== "macos") return [];
  return [
    {
      key: "accessibility",
      title: "Accessibility",
      status: permissionStatus(snapshot.permissions.accessibility),
      recovery: permissionRecovery("Accessibility", snapshot.permissions.accessibility),
      why: "Allows keyboard and mouse control when sharing is on.",
      pane: "accessibility",
    },
    {
      key: "inputMonitoring",
      title: "Input Monitoring",
      status: permissionStatus(snapshot.permissions.inputMonitoring),
      recovery: permissionRecovery("Input Monitoring", snapshot.permissions.inputMonitoring),
      why: "Reads keyboard and mouse activity. Setup records nothing.",
      pane: "input-monitoring",
    },
  ];
}

function permissionStatus(value) {
  if (value === true) return { label: "Allowed", tone: "granted" };
  if (value === false) return { label: "Not allowed", tone: "denied" };
  return { label: "Unknown", tone: "unknown" };
}

export function accessReady(snapshot) {
  if (!snapshot || snapshot.errors.length) return false;
  if (snapshot.platform === "windows") return true;
  if (snapshot.platform !== "macos") return false;
  return (
    snapshot.permissions.accessibility === true && snapshot.permissions.inputMonitoring === true
  );
}

export function networkReady(state) {
  const selected = selectedInterface(state);
  return selected?.physical === true && selected.up === true && selected.attachmentKnown === true;
}

function eligibleInterfaces(snapshot) {
  return (snapshot?.interfaces ?? []).filter(canSelectInterface);
}

// The network chosen last time comes back when it is still usable; otherwise one eligible
// recognized network needs no choice, and the user still sees which one.
export function autoSelectInterface(state, preferredId = null) {
  if (state.selectedInterfaceId) return state;
  const preferred = selectInterface(state, preferredId);
  if (preferred.selectedInterfaceId) return preferred;
  const eligible = eligibleInterfaces(state.snapshot).filter((item) => item.attachmentKnown);
  return eligible.length === 1 ? selectInterface(state, eligible[0].id) : state;
}

function permissionRecovery(title, value) {
  if (value !== false) return null;
  return {
    summary: `Fix ${title} in Settings`,
    pane: `System Settings > Privacy & Security > ${title}`,
    restartNote:
      title === "Input Monitoring"
        ? "Input Monitoring can require quitting and reopening MonHop."
        : null,
  };
}

export function wifiAuthorizationStatus(value) {
  switch (wifiAuthorization(value)) {
    case "not-determined":
      return { label: "Not asked yet", tone: "needed" };
    case "denied":
      return { label: "Not allowed", tone: "denied" };
    case "restricted":
      return { label: "Restricted", tone: "denied" };
    case "authorized":
      return { label: "Allowed", tone: "granted" };
    case "services-disabled":
      return { label: "Location Services off", tone: "needed" };
    default:
      return { label: "Unknown", tone: "unknown" };
  }
}

export function hasRelevantWifiInterface(snapshot) {
  return (
    snapshot?.platform === "macos" &&
    snapshot.interfaces.some((item) => item.physical && item.up && item.kind === "Wi-Fi")
  );
}

export function wifiRecognition(snapshot) {
  const access = wifiAuthorization(snapshot?.permissions?.wifiAuthorization);
  const recognized =
    snapshot?.interfaces?.some(
      (item) => item.physical && item.up && item.kind === "Wi-Fi" && item.attachmentKnown,
    ) === true;
  if (recognized) {
    return {
      label: "Recognized",
      detail: "MonHop can identify this network when checking whether it has changed.",
    };
  }
  if (access === "authorized") {
    return {
      label: "Not recognized",
      detail:
        "Access is allowed, but the network is still unknown. Check again. If it stays unknown, this build cannot use it yet.",
    };
  }
  if (access === "denied" || access === "restricted") {
    return {
      label: "Not recognized",
      detail: "Location access is not allowed, so MonHop cannot recognize this Wi-Fi network yet.",
    };
  }
  if (access === "services-disabled") {
    return {
      label: "Not recognized",
      detail: "Location Services are off, so MonHop cannot recognize this Wi-Fi network yet.",
    };
  }
  return {
    label: "Not recognized",
    detail:
      "Location access has not been confirmed, so MonHop cannot recognize this Wi-Fi network yet.",
  };
}

export function canSelectInterface(item) {
  return item?.physical === true && item?.up === true && Boolean(item.id);
}

export function selectInterface(state, id) {
  const item = state.snapshot?.interfaces.find((candidate) => candidate.id === id);
  return {
    ...state,
    selectedInterfaceId: canSelectInterface(item) ? item.id : null,
  };
}

export function selectedInterface(state) {
  return state.snapshot?.interfaces.find((item) => item.id === state.selectedInterfaceId) ?? null;
}

export function pairingOpenGate(state) {
  if (!state.nativeAvailable) {
    return {
      allowed: false,
      detail: "Open MonHop to pair computers. A browser preview cannot access saved identities.",
    };
  }
  const snapshot = state.snapshot;
  if (!snapshot) {
    return { allowed: false, detail: "Finish Get ready first." };
  }
  if (snapshot.platform === "unsupported") {
    return { allowed: false, detail: "Pairing is not supported on this computer." };
  }
  if (!snapshot.pairingAvailable) {
    return { allowed: false, detail: "This build cannot pair computers." };
  }
  if (snapshot.errors.length) {
    return { allowed: false, detail: "Fix the check that needs attention in Get ready first." };
  }
  const selected = selectedInterface(state);
  if (!selected?.physical || !selected.up || !selected.attachmentKnown) {
    return { allowed: false, detail: "Choose a connected, recognized network in Get ready first." };
  }
  return {
    allowed: true,
    detail: "Reads this computer's saved identity. macOS may ask to unlock it.",
  };
}

export function interfaceAssessment(item) {
  if (!item.physical) return "Not a hardware network";
  if (!item.up) return "Not connected";
  if (!item.attachmentKnown) return "Network not recognized";
  return "Network recognized";
}

export function networkLabel(item) {
  const kind = item.kind.trim().toLowerCase();
  if (kind === "wi-fi" || kind === "wifi") return item.networkName || "Wi-Fi";
  if (kind === "ethernet") return item.name || "Ethernet";
  return item.name || "Other network";
}

export function networkDetail(item) {
  const kind = item.kind.trim().toLowerCase();
  const wifi = kind === "wi-fi" || kind === "wifi";
  const connection = wifi ? "Wi-Fi" : kind === "ethernet" ? "Ethernet" : "";
  const nameUnavailable = wifi && item.up && item.physical && !item.networkName;
  return [connection, interfaceAssessment(item), nameUnavailable ? "Name unavailable" : ""]
    .filter(Boolean)
    .join(" · ");
}

export function interfaceAuthorizationGate(item) {
  if (!item?.physical) {
    return {
      label: "Not available",
      tone: "needed",
      detail: "Only a connected hardware network can be checked.",
    };
  }
  if (!item.up) {
    return {
      label: "Not connected",
      tone: "needed",
      detail: "Connect this network before checking it.",
    };
  }
  if (!item.attachmentKnown) {
    return {
      label: "Not recognized",
      tone: "needed",
      detail: "MonHop cannot confirm this network yet. Pairing stays off.",
    };
  }
  return {
    label: "Ready to inspect",
    tone: "unknown",
    detail: "This can be checked locally. It does not pair computers.",
  };
}

export function errorMessages(state) {
  const snapshotErrors = state.snapshot?.errors ?? [];
  return [...snapshotErrors, ...state.messages].filter(Boolean);
}

export function setupVerdict(state, { checking = false } = {}) {
  if (!state.nativeAvailable) {
    return { label: "Preview only", detail: "Open MonHop to check this computer.", tone: "wait" };
  }
  if (state.actionFailed) {
    return {
      label: "Something did not finish",
      detail: "See the message above, then try again.",
      tone: "fail",
    };
  }
  if (!state.snapshot) {
    return checking
      ? { label: "Checking this computer", detail: "This takes a moment.", tone: "wait" }
      : {
          label: "Not checked yet",
          detail: "Use the refresh button to check this computer.",
          tone: "wait",
        };
  }
  if (state.snapshot.platform === "unsupported") {
    return {
      label: "This computer is not supported",
      detail: "MonHop runs on macOS and Windows.",
      tone: "fail",
    };
  }
  if (state.snapshot.errors.length) {
    return {
      label: "A check needs attention",
      detail: "Some information could not be read. See the message above.",
      tone: "fail",
    };
  }
  if (!accessReady(state.snapshot)) {
    return {
      label: "Access needed",
      detail: "Allow the permissions below, then MonHop checks again.",
      tone: "fail",
    };
  }
  if (!networkReady(state)) {
    return {
      label: "Choose a network",
      detail: "Pick the network both computers share.",
      tone: "wait",
    };
  }
  return {
    label: "Ready",
    detail: `Using ${networkLabel(selectedInterface(state))}. Pair a computer below.`,
    tone: "done",
  };
}

function normalizeInterface(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    id: text(source.id),
    index: numberOrZero(source.index),
    name: displayLabel(source.name, 256),
    networkName: displayLabel(source.networkName, 32),
    address: text(source.address),
    prefixLength: numberOrZero(source.prefixLength),
    kind: text(source.kind),
    physical: source.physical === true,
    up: source.up === true,
    attachmentKnown: source.attachmentKnown === true,
  };
}

function normalizeDisplay(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    name: text(source.name),
    width: numberOrZero(source.width),
    height: numberOrZero(source.height),
    primary: source.primary === true,
  };
}

function text(value) {
  return typeof value === "string" ? value : "";
}

function displayLabel(value, maxBytes) {
  const label = text(value);
  if (
    !label.trim() ||
    new TextEncoder().encode(label).length > maxBytes ||
    // oxlint-disable-next-line no-control-regex -- strips control/bidi-override chars by design
    /[\u0000-\u001f\u007f-\u009f\u061c\u200e\u200f\u2028-\u202e\u2066-\u2069]/u.test(label)
  )
    return "";
  return label;
}

function array(value) {
  return Array.isArray(value) ? value : [];
}

function booleanOrNull(value) {
  return value === true || value === false ? value : null;
}

function wifiAuthorization(value) {
  return WIFI_AUTHORIZATION.has(value) ? value : "unknown";
}

function numberOrZero(value) {
  return Number.isFinite(value) && value >= 0 ? Math.trunc(value) : 0;
}
