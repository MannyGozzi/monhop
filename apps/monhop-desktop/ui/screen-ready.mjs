import {
  accessReady,
  canSelectInterface,
  hasRelevantWifiInterface,
  interfaceAuthorizationGate,
  networkDetail,
  networkLabel,
  readinessCount,
  readinessItems,
  selectedInterface,
  setupVerdict,
  wifiAuthorizationStatus,
  wifiRecognition,
} from "./model.mjs";
import { createAccordion, presence } from "./accordion.mjs";
import { isSessionActive } from "./sharing-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import { NETWORK_LOCKED } from "./studio-model.mjs";
import {
  badge,
  button,
  card,
  el,
  facts,
  icon,
  note,
  platformGlyph,
  radioMark,
  row,
  rows,
  sinceChanged,
  text,
  iconButton,
  motionMs,
} from "./dom.mjs";

const UNRECOGNIZED =
  "This network is not recognized yet, so MonHop cannot pin the connection to it.";

// This Mac's inspector: how ready it is, the Get ready checklist with each fix in place, the way to
// add a computer, and how it was launched.
export function localInspector(ctx) {
  const { state, snapshotCheck, actions, platform } = ctx;
  const verdict = setupVerdict(state, {
    checking: snapshotCheck.pending,
    paired: ctx.computers.items.length > 0,
  });
  const localPlatform = state.snapshot?.platform ?? platform;
  const version = state.snapshot?.version || ctx.updates.view.currentVersion;
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
      el("section", {
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
        ],
      }),
      readiness(ctx),
      addComputer(ctx),
      createAccordion("overview-details", "launch-details", "Details", launchAttribution(ctx)),
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

// One heading over a block of the inspector, with an optional note on its right.
function section(title, aside, children, { className = "", id } = {}) {
  return el("section", {
    className: `inspector-section ${className}`.trim(),
    attrs: { id, "aria-label": title },
    children: [
      el("div", {
        className: "inspector-section-heading",
        children: [
          el("h3", { text: title }),
          aside ? el("span", { className: "inspector-section-aside", text: aside }) : null,
        ],
      }),
      ...children,
    ],
  });
}

// ---------- Get ready ----------

function readiness(ctx) {
  const { state } = ctx;
  if (!state.nativeAvailable)
    return card({
      title: "Open MonHop to continue",
      description: "The browser preview cannot check this computer.",
      tone: "muted",
    });
  const snapshot = state.snapshot;
  if (!snapshot) return null;
  if (snapshot.platform === "unsupported")
    return card({
      title: "This computer is not supported",
      description: "MonHop runs on macOS and Windows.",
      tone: "danger",
    });
  const items = readinessItems(state);
  const access = items.filter((item) => item.key !== "network");
  const network = items.find((item) => item.key === "network");
  const firstOpen = access.find((item) => !item.done)?.key ?? null;
  const tutorial =
    snapshot.platform === "macos" && !accessReady(snapshot)
      ? createAccordion(
          "permission-tutorial",
          "tutorial",
          "Where are these macOS settings?",
          settingsMock(),
        )
      : null;
  return section("Get ready", readinessCount(items), [
    el("div", {
      className: "ready-list",
      attrs: { id: "permissions-content" },
      children: [
        ...access.map((item) =>
          presence(`ready-${item.key}`, permissionItem(ctx, item, item.key === firstOpen)),
        ),
        presence("ready-tutorial", tutorial),
      ],
    }),
    el("div", {
      className: "ready-list",
      attrs: { id: "interface-content" },
      children: [presence("ready-network", networkItem(ctx, network))],
    }),
  ]);
}

// A checklist row: a round mark that fills in once done, the title and its state on one line, and
// whatever fixes it underneath, which folds away once it is done.
function readyItem(item, { trailing = null, summary = null, fix = null }) {
  const elapsed = sinceChanged(`ready-mark-${item.key}`, item.done);
  const mark = el("span", {
    className: "ready-mark",
    attrs: { "aria-hidden": "true" },
    children: [icon("check", 12)],
  });
  if (elapsed < motionMs("--motion-spring")) {
    mark.dataset.enter = "true";
    mark.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
  }
  return el("div", {
    className: "ready-item",
    dataset: { done: String(item.done), key: item.key },
    children: [
      mark,
      el("div", {
        className: "ready-body",
        children: [
          el("div", {
            className: "ready-line",
            children: [
              el("strong", { className: "ready-title", text: item.title }),
              trailing ??
                el("span", {
                  className: "ready-status",
                  text: item.status,
                  attrs: { "aria-label": `${item.title}: ${item.status}` },
                }),
            ],
          }),
          summary,
          presence(`ready-${item.key}-fix`, fix),
        ],
      }),
    ],
  });
}

function permissionItem(ctx, item, first) {
  const { actions, busy, state } = ctx;
  if (item.key === "windows")
    return readyItem(item, { summary: el("p", { className: "ready-note", text: item.detail }) });
  const needsAccess = !accessReady(state.snapshot) && !state.snapshot.errors.length;
  const fix = item.done
    ? null
    : el("div", {
        className: "ready-fix",
        children: [
          el("p", { className: "ready-note", text: item.detail }),
          el("div", {
            className: "ready-actions",
            children: [
              button("Open Settings", {
                size: "sm",
                disabled: busy,
                focusKey: `ready-open-${item.key}`,
                onClick: () => actions.openSettings(item.pane),
              }),
              needsAccess
                ? button("Request access", {
                    variant: "ghost",
                    size: "sm",
                    disabled: busy,
                    focusKey: `ready-request-${item.key}`,
                    onClick: actions.requestPermissions,
                  })
                : null,
            ],
          }),
          first && needsAccess
            ? el("p", { className: "ready-note", text: "You approve each macOS prompt." })
            : null,
          item.recovery ? recoveryDisclosure(item.recovery) : null,
        ],
      });
  return readyItem(item, { fix });
}

// Done, the network is one line with the connection under it and Change to pick another; until
// then the whole list is open under it.
function networkItem(ctx, item) {
  const { actions, busy, sharing } = ctx;
  const linked = isSessionActive(sharing);
  const wifi = wifiFix(ctx);
  const open = !item.done || ctx.localNetworkOpen;
  const selection = selectedInterface(ctx.state);
  const change = item.done
    ? button(ctx.localNetworkOpen ? "Done" : "Change", {
        variant: "ghost",
        size: "sm",
        disabled: busy,
        focusKey: "ready-network-change",
        pressed: ctx.localNetworkOpen,
        onClick: actions.toggleLocalNetworkList,
      })
    : null;
  const fix = el("div", {
    className: "ready-fix",
    children: [
      presence(
        "ready-network-list",
        open
          ? el("div", {
              className: "ready-fix",
              children: [
                el("p", {
                  className: "ready-note",
                  text: linked ? NETWORK_LOCKED : networkHint(ctx),
                }),
                ...networkList(ctx, "ready-network"),
                selection && !selection.attachmentKnown
                  ? el("p", {
                      className: "ready-note",
                      dataset: { tone: "warning" },
                      text: UNRECOGNIZED,
                    })
                  : null,
              ],
            })
          : null,
      ),
      presence("ready-network-wifi", wifi),
    ],
  });
  return readyItem(item, {
    trailing: change,
    summary:
      item.done || selection ? el("p", { className: "ready-value", text: item.detail }) : null,
    fix: open || wifi ? fix : null,
  });
}

function networkHint(ctx) {
  const eligible = ctx.state.snapshot.interfaces.filter(canSelectInterface);
  return eligible.length > 1
    ? "Both computers must be on the same network."
    : "Both computers must be on this network.";
}

// The Wi-Fi network is recognized through Location access; until it is, the way to allow it.
function wifiFix(ctx) {
  const parts = wifiParts(ctx, "ready-wifi");
  if (!parts) return null;
  return el("div", {
    className: "ready-wifi",
    children: [
      el("div", {
        className: "ready-line",
        children: [
          el("strong", { className: "ready-subtitle", text: parts.title }),
          badge(parts.status.label, parts.status.tone),
        ],
      }),
      el("p", { className: "ready-note", text: parts.detail }),
      parts.action ? el("div", { className: "ready-actions", children: [parts.action] }) : null,
    ],
  });
}

// `scope` keeps each place's focus keys apart: This Mac and Settings both draw these controls.
function wifiParts(ctx, scope) {
  const { state, actions, busy } = ctx;
  const snapshot = state.snapshot;
  if (!hasRelevantWifiInterface(snapshot)) return null;
  const authorization = snapshot.permissions.wifiAuthorization;
  const recognition = wifiRecognition(snapshot);
  if (authorization === "authorized" && recognition.label === "Recognized") return null;
  const action =
    authorization === "not-determined"
      ? button("Allow location access", {
          size: "sm",
          disabled: busy,
          focusKey: `${scope}-allow`,
          onClick: actions.requestWifiPermission,
        })
      : authorization !== "authorized"
        ? button("Open Location Settings", {
            variant: "secondary",
            size: "sm",
            disabled: busy,
            focusKey: `${scope}-settings`,
            onClick: () => actions.openSettings("location-services"),
          })
        : null;
  return {
    title: "Recognize this Wi-Fi network",
    detail: `${recognition.detail} MonHop never stores your location.`,
    status: wifiAuthorizationStatus(authorization),
    action,
  };
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

// ---------- Add a computer and Details ----------

// Show a code or Enter a code, from the same place a first run lands.
function addComputer(ctx) {
  const { actions, busy, gate } = ctx;
  return section("Add a computer", null, [
    el("p", {
      className: "inspector-section-lead",
      text: gate.allowed
        ? "Show a short code on one computer and type it on the other."
        : gate.detail,
    }),
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
  ]);
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

// ---------- the network list, here and in Settings ----------

// The networks MonHop may use as radio rows, and the ones it cannot under a disclosure.
// While a session runs the rows keep the chosen network checked but take no press.
function networkList(ctx, scope) {
  const { state, actions, sharing } = ctx;
  const locked = isSessionActive(sharing);
  const interfaces = state.snapshot.interfaces;
  const eligible = interfaces.filter(canSelectInterface);
  const unavailable = interfaces.filter((item) => !canSelectInterface(item));
  const children = [];
  if (eligible.length === 0)
    children.push(
      note("No connected Wi-Fi or Ethernet network was found. Connect one, then check again."),
    );
  else
    children.push(
      rows(
        eligible.map((item) => {
          const gate = interfaceAuthorizationGate(item);
          return row({
            title: networkLabel(item),
            detail: `${networkDetail(item)}${item.address ? ` · ${item.address}` : ""}`,
            leading: radioMark(),
            selectable: true,
            disabled: locked,
            checked: state.selectedInterfaceId === item.id,
            focusKey: `${scope}-${item.id}`,
            onSelect: () => actions.chooseInterface(item.id),
            actions: [gate.tone === "needed" ? badge(gate.label, "needed") : null].filter(Boolean),
          });
        }),
      ),
    );
  if (unavailable.length)
    children.push(
      createAccordion(
        "other-networks",
        "other-networks",
        `${unavailable.length} other network${unavailable.length === 1 ? "" : "s"} MonHop cannot use`,
        facts(unavailable.map((item) => [networkLabel(item), networkDetail(item)])),
      ),
    );
  return children;
}

// The network card Settings shows: the same list, the Wi-Fi row and the lock while sharing.
export function networkCard(ctx) {
  const { state, sharing } = ctx;
  const snapshot = state.snapshot;
  if (!state.nativeAvailable || !snapshot || snapshot.platform === "unsupported") return null;
  const children = networkList(ctx, "network");
  const wifi = wifiParts(ctx, "settings-wifi");
  if (wifi)
    children.push(
      rows([
        row({
          title: wifi.title,
          detail: wifi.detail,
          actions: [badge(wifi.status.label, wifi.status.tone), wifi.action].filter(Boolean),
        }),
      ]),
    );
  const selection = selectedInterface(state);
  return card({
    title: "Network",
    description: isSessionActive(sharing) ? NETWORK_LOCKED : networkHint(ctx),
    children,
    actions: selection && !selection.attachmentKnown ? [note(UNRECOGNIZED)] : [],
  });
}
