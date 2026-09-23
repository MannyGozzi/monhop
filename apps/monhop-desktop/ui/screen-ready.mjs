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
import {
  badge,
  button,
  card,
  clear,
  el,
  facts,
  iconButton,
  note,
  radioMark,
  row,
  rows,
  text,
} from "./dom.mjs";

export function renderReady(nodes, ctx) {
  const { state, snapshotCheck, actions } = ctx;
  const verdict = setupVerdict(state, { checking: snapshotCheck.pending });
  nodes.setupVerdict.textContent = verdict.label;
  nodes.setupDetail.textContent = verdict.detail;
  nodes.setupVerdict.closest(".overview").dataset.tone = verdict.tone;
  renderLaunchAttribution(nodes, ctx);
  clear(nodes.readyActions);
  nodes.readyActions.append(
    iconButton({
      id: "permissions-refresh",
      label: "Check this computer again",
      size: "sm",
      disabled: !state.nativeAvailable || ctx.busy,
      busy: snapshotCheck.pending,
      onClick: actions.refreshSnapshot,
    }),
  );
  renderAccess(nodes, ctx);
  renderNetworks(nodes, ctx);
}

function renderLaunchAttribution(nodes, ctx) {
  clear(nodes.launchAttribution);
  const launch = ctx.state.snapshot?.launch;
  if (!launch?.executable) {
    nodes.launchAttribution.textContent = ctx.state.nativeAvailable
      ? "Available after the first check."
      : "The browser preview cannot report this.";
    return;
  }
  nodes.launchAttribution.append(
    text("Checked app: "),
    el("code", { text: launch.executable }),
    text(launch.bundled ? " · app bundle" : " · external launch"),
  );
  if (ctx.state.snapshot.platform === "macos")
    nodes.launchAttribution.append(
      el("small", {
        text: "macOS permissions belong to the app you launched. A Terminal launch may use the launcher's entry instead.",
      }),
    );
  if (ctx.state.snapshot.version)
    nodes.launchAttribution.append(el("small", { text: `Version ${ctx.state.snapshot.version}` }));
}

function renderAccess(nodes, ctx) {
  const { state, actions, busy } = ctx;
  clear(nodes.permissionsContent);
  const snapshot = state.snapshot;
  if (!state.nativeAvailable) {
    nodes.permissionsContent.append(
      card({
        title: "Open MonHop to continue",
        description: "The browser preview cannot check this computer.",
        tone: "muted",
      }),
    );
    return;
  }
  if (!snapshot) return;
  if (snapshot.platform === "unsupported") {
    nodes.permissionsContent.append(
      card({
        title: "This computer is not supported",
        description: "MonHop runs on macOS and Windows.",
        tone: "danger",
      }),
    );
    return;
  }
  if (snapshot.platform === "windows") {
    nodes.permissionsContent.append(
      card({
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
      }),
    );
    return;
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
  nodes.permissionsContent.append(
    card({
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
    }),
  );
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

function renderNetworks(nodes, ctx) {
  const { state, actions, sharing } = ctx;
  clear(nodes.interfaceContent);
  const snapshot = state.snapshot;
  if (!state.nativeAvailable || !snapshot || snapshot.platform === "unsupported") return;
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
    ? "Pause the computer in use before changing the network."
    : eligible.length > 1
      ? "Both computers must be on the same network."
      : "Both computers must be on this network.";
  nodes.interfaceContent.append(
    card({
      title: "Network",
      description,
      children,
      actions:
        selection && !selection.attachmentKnown
          ? [note("This network is not recognized yet, so MonHop cannot pin the connection to it.")]
          : [],
    }),
  );
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

function isSelectable(item) {
  return item.physical === true && item.up === true && Boolean(item.id);
}
