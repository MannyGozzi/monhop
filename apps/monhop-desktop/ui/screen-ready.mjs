import {
  accessReady,
  hasRelevantWifiInterface,
  interfaceAuthorizationGate,
  networkDetail,
  networkLabel,
  permissionRows,
  selectedInterface,
  setupVerdict,
  wifiAuthorizationStatus,
  wifiRecognition,
} from "./model.mjs";
import { createAccordion } from "./accordion.mjs";
import { isSessionActive } from "./sharing-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import { NETWORK_LOCKED } from "./studio-model.mjs";
import {
  badge,
  button,
  card,
  el,
  facts,
  iconButton,
  note,
  platformGlyph,
  radioMark,
  row,
  rows,
  text,
} from "./dom.mjs";

// This Mac's inspector: getting ready (access and the network), how it was launched, and the way
// to add a computer.
export function localInspector(ctx) {
  const { state, snapshotCheck, actions, platform } = ctx;
  const verdict = setupVerdict(state, { checking: snapshotCheck.pending });
  const localPlatform = state.snapshot?.platform ?? platform;
  const version = state.snapshot?.version || ctx.updates.view.currentVersion;
  const overview = el("section", {
    className: "overview",
    dataset: { tone: verdict.tone },
    attrs: { "aria-label": "Setup status" },
    children: [
      el("div", {
        className: "overview-copy",
        children: [
          el("strong", { text: verdict.label, attrs: { id: "setup-verdict" } }),
          el("span", { text: verdict.detail, attrs: { id: "setup-detail" } }),
        ],
      }),
      createAccordion("overview-details", "launch-details", "Details", launchAttribution(ctx)),
    ],
  });
  return el("div", {
    className: "inspector-view",
    dataset: { kind: "local" },
    children: [
      inspectorHeader({
        glyph: platformGlyph(localPlatform === "windows" ? "windows" : "macos"),
        tone: "local",
        title: platformLabel(localPlatform, true),
        meta: [platformName(localPlatform), version ? `MonHop ${version}` : ""]
          .filter(Boolean)
          .join(" · "),
        actions: [
          iconButton({
            id: "permissions-refresh",
            label: "Check this computer again",
            size: "sm",
            variant: "ghost",
            disabled: !state.nativeAvailable || ctx.busy,
            busy: snapshotCheck.pending,
            onClick: actions.refreshSnapshot,
          }),
        ],
      }),
      overview,
      // Once this computer is ready, adding a computer is the next thing to do, so it comes first.
      verdict.tone === "done" ? addComputerCard(ctx) : null,
      el("div", {
        className: "stack",
        attrs: { id: "permissions-content" },
        children: [access(ctx)],
      }),
      el("div", {
        className: "stack",
        attrs: { id: "interface-content" },
        children: [networkCard(ctx)],
      }),
      verdict.tone === "done" ? null : addComputerCard(ctx),
    ],
  });
}

// Every inspector opens the same way: the computer's tile, its name and one line about it.
export function inspectorHeader({
  glyph,
  tone,
  title,
  titleNode = null,
  meta,
  metaNode = null,
  actions = [],
}) {
  return el("header", {
    className: "inspector-header",
    children: [
      el("span", {
        className: "inspector-tile",
        dataset: { tone },
        attrs: { "aria-hidden": "true" },
        children: [glyph],
      }),
      el("div", {
        className: "inspector-heading",
        children: [
          titleNode ??
            el("h2", {
              className: "inspector-title",
              text: title,
              attrs: { id: "inspector-title" },
            }),
          metaNode ?? el("p", { className: "inspector-meta", text: meta }),
        ],
      }),
      actions.length
        ? el("div", { className: "inspector-header-actions", children: actions })
        : null,
    ],
  });
}

function platformName(platform) {
  return platform === "windows" ? "Windows" : platform === "macos" ? "macOS" : "";
}

// Show a code or Enter a code, from the same place a first run lands.
function addComputerCard(ctx) {
  const { actions, busy, gate } = ctx;
  return card({
    title: "Add a computer",
    description: gate.allowed
      ? "Show a short code on one computer and type it on the other."
      : gate.detail,
    children: [
      el("div", {
        className: "pair-choices",
        children: [
          button("Show a code", {
            disabled: !gate.allowed || busy,
            focusKey: "local-pairing-show",
            onClick: () => actions.startPairing("show"),
          }),
          button("Enter a code", {
            variant: "secondary",
            disabled: !gate.allowed || busy,
            focusKey: "local-pairing-enter",
            onClick: () => actions.startPairing("enter"),
          }),
        ],
      }),
    ],
  });
}

function launchAttribution(ctx) {
  const node = el("div", { className: "launch-attribution", attrs: { id: "launch-attribution" } });
  const launch = ctx.state.snapshot?.launch;
  if (!launch?.executable) {
    node.textContent = ctx.state.nativeAvailable
      ? "Available after the first check."
      : "The browser preview cannot report this.";
    return node;
  }
  node.append(
    text("Checked app: "),
    el("code", { text: launch.executable }),
    text(launch.bundled ? " · app bundle" : " · external launch"),
  );
  if (ctx.state.snapshot.platform === "macos")
    node.append(
      el("small", {
        text: "macOS permissions belong to the app you launched. A Terminal launch may use the launcher's entry instead.",
      }),
    );
  if (ctx.state.snapshot.version)
    node.append(el("small", { text: `Version ${ctx.state.snapshot.version}` }));
  return node;
}

function access(ctx) {
  const { state, actions, busy } = ctx;
  const snapshot = state.snapshot;
  if (!state.nativeAvailable)
    return card({
      title: "Open MonHop to continue",
      description: "The browser preview cannot check this computer.",
      tone: "muted",
    });
  if (!snapshot) return null;
  if (snapshot.platform === "unsupported")
    return card({
      title: "This computer is not supported",
      description: "MonHop runs on macOS and Windows.",
      tone: "danger",
    });
  if (snapshot.platform === "windows") {
    return card({
      title: "Access",
      description:
        "Windows needs no extra permission for MonHop. No administrator access is requested.",
      children: [
        rows([
          row({
            title: "Windows check",
            detail: "Secure and elevated screens stay outside MonHop.",
            actions: [badge("Ready", "success", "check")],
          }),
        ]),
      ],
    });
  }
  const items = permissionRows(snapshot).map((item) =>
    row({
      title: item.title,
      detail: item.why,
      actions: [
        badge(item.status.label, item.status.tone, item.status.tone === "granted" ? "check" : null),
        item.status.tone === "granted"
          ? null
          : button("Open Settings", {
              variant: "outline",
              size: "sm",
              disabled: busy,
              onClick: () => actions.openSettings(item.pane),
            }),
      ].filter(Boolean),
    }),
  );
  const needsAccess = !accessReady(snapshot) && !snapshot.errors.length;
  const children = [rows(items)];
  const recovery = permissionRows(snapshot).filter((item) => item.recovery);
  for (const item of recovery) children.push(recoveryDisclosure(item.recovery));
  children.push(
    createAccordion(
      "permission-tutorial",
      "tutorial",
      "Where are these macOS settings?",
      settingsMock(),
    ),
  );
  return card({
    title: "Access",
    description:
      "macOS asks once for Accessibility and Input Monitoring. MonHop records nothing during setup.",
    children,
    actions: needsAccess
      ? [
          note("You approve each macOS prompt."),
          button("Request access", {
            variant: "outline",
            disabled: busy,
            onClick: actions.requestPermissions,
          }),
        ]
      : [],
  });
}

function recoveryDisclosure(recovery) {
  const steps = el("ol", {
    className: "note recovery-steps",
    children: [
      el("li", { text: `Open ${recovery.pane}.` }),
      el("li", {
        text: "If MonHop is already switched on, quit MonHop, remove its entry with the minus button, then add /Applications/MonHop.app again.",
      }),
      el("li", { text: "Switch MonHop on." }),
      el("li", { text: "Reopen MonHop. It checks again automatically." }),
    ],
  });
  const content = [note("Updated development builds can leave a stale entry behind."), steps];
  if (recovery.restartNote) content.push(note(recovery.restartNote));
  return createAccordion(
    `permission-recovery-${recovery.pane}`,
    "permission-recovery",
    recovery.summary,
    ...content,
  );
}

function settingsMock() {
  const mock = el("div", { className: "settings-mock", attrs: { "aria-hidden": "true" } });
  mock.append(
    el("div", {
      className: "mock-title",
      children: [el("i"), el("i"), el("i"), el("b", { text: "Privacy & Security" })],
    }),
    el("div", {
      className: "mock-body",
      children: [
        el("div", {
          className: "mock-sidebar",
          children: [
            el("span", { text: "Accessibility" }),
            el("span", { text: "Input Monitoring" }),
          ],
        }),
        el("div", {
          className: "mock-panes",
          children: [
            el("div", {
              className: "mock-permission-pane",
              children: [
                el("strong", { text: "Accessibility" }),
                el("div", {
                  className: "mock-row",
                  children: [
                    el("span", { text: "MonHop" }),
                    el("em", { className: "mock-switch" }),
                  ],
                }),
              ],
            }),
            el("div", {
              className: "mock-permission-pane delayed",
              children: [
                el("strong", { text: "Input Monitoring" }),
                el("div", {
                  className: "mock-row",
                  children: [
                    el("span", { text: "MonHop" }),
                    el("em", { className: "mock-switch" }),
                  ],
                }),
              ],
            }),
          ],
        }),
      ],
    }),
  );
  return el("div", {
    className: "tutorial-content",
    children: [note("Look for MonHop in both lists and switch it on."), mock],
  });
}

// The network list, here and in Settings: the one connection MonHop may use, locked while sharing.
export function networkCard(ctx) {
  const { state, actions, sharing } = ctx;
  const snapshot = state.snapshot;
  if (!state.nativeAvailable || !snapshot || snapshot.platform === "unsupported") return null;
  const linked = isSessionActive(sharing);
  const eligible = snapshot.interfaces.filter(isSelectable);
  const unavailable = snapshot.interfaces.filter((item) => !isSelectable(item));
  const children = [];
  if (eligible.length === 0) {
    children.push(
      note("No connected Wi-Fi or Ethernet network was found. Connect one, then check again."),
    );
  } else {
    children.push(
      rows(
        eligible.map((item) => {
          const gate = interfaceAuthorizationGate(item);
          const selected = state.selectedInterfaceId === item.id;
          return row({
            title: networkLabel(item),
            detail: `${networkDetail(item)}${item.address ? ` · ${item.address}` : ""}`,
            leading: radioMark(),
            selectable: true,
            checked: selected,
            focusKey: `network-${item.id}`,
            onSelect: () => actions.chooseInterface(item.id),
            actions: [gate.tone === "needed" ? badge(gate.label, "needed") : null].filter(Boolean),
          });
        }),
      ),
    );
  }
  if (unavailable.length) {
    const list = facts(unavailable.map((item) => [networkLabel(item), networkDetail(item)]));
    children.push(
      createAccordion(
        "other-networks",
        "other-networks",
        `${unavailable.length} other network${unavailable.length === 1 ? "" : "s"} MonHop cannot use`,
        list,
      ),
    );
  }
  const wifi = wifiCard(ctx);
  if (wifi) children.push(wifi);
  const selection = selectedInterface(state);
  const description = linked
    ? NETWORK_LOCKED
    : eligible.length > 1
      ? "Both computers must be on the same network."
      : "Both computers must be on this network.";
  return card({
    title: "Network",
    description,
    children,
    actions:
      selection && !selection.attachmentKnown
        ? [note("This network is not recognized yet, so MonHop cannot pin the connection to it.")]
        : [],
  });
}

function wifiCard(ctx) {
  const { state, actions, busy } = ctx;
  const snapshot = state.snapshot;
  if (!hasRelevantWifiInterface(snapshot)) return null;
  const status = wifiAuthorizationStatus(snapshot.permissions.wifiAuthorization);
  const recognition = wifiRecognition(snapshot);
  if (snapshot.permissions.wifiAuthorization === "authorized" && recognition.label === "Recognized")
    return null;
  const actionsList = [];
  if (snapshot.permissions.wifiAuthorization === "not-determined")
    actionsList.push(
      button("Allow location access", {
        variant: "outline",
        size: "sm",
        disabled: busy,
        onClick: actions.requestWifiPermission,
      }),
    );
  else if (snapshot.permissions.wifiAuthorization !== "authorized")
    actionsList.push(
      button("Open Location Settings", {
        variant: "outline",
        size: "sm",
        disabled: busy,
        onClick: () => actions.openSettings("location-services"),
      }),
    );
  return el("div", {
    className: "rows",
    children: [
      row({
        title: "Recognize this Wi-Fi network",
        detail: `${recognition.detail} MonHop never stores your location.`,
        actions: [badge(status.label, status.tone), ...actionsList],
      }),
    ],
  });
}

export function isSelectable(item) {
  return item.physical === true && item.up === true && Boolean(item.id);
}
