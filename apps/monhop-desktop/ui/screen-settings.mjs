import { canCheck, canInstall, installHint, updatesStatusText } from "./updates-model.mjs";
import {
  autostartDescription,
  autostartOpenLabel,
  autostartStatusText,
  showAutostartOpenSettings,
} from "./autostart-model.mjs";
import { ISLAND_DESCRIPTION, ISLAND_SWITCH_LABEL, ISLAND_TITLE } from "./island-model.mjs";
import {
  DIM_LEVEL_STEP,
  dimButtonLabel,
  dimmingMessage,
  levelLabel,
  shortcutDescription,
} from "./dimming-model.mjs";
import { AUTOSCROLL_LABEL, AUTOSCROLL_NOTE } from "./autoscroll-model.mjs";
import { SWIPE_LABEL, SWIPE_NOTE } from "./swipe-model.mjs";
import { CONTROL_AS_COMMAND_LABEL, CONTROL_AS_COMMAND_NOTE } from "./keyboard-model.mjs";
import {
  CLIPBOARD_PRIVACY,
  clipboardAccessNotice,
  clipboardNoticeText,
  clipboardPeerLines,
  clipboardPill,
  clipboardStatusText,
  lastTransferText,
  normalizeClipboardView,
} from "./clipboard-model.mjs";
import { presence } from "./accordion.mjs";
import { networkCard } from "./screen-ready.mjs";
import {
  button,
  clear,
  el,
  note,
  progressBar,
  row,
  rows,
  sliderRow,
  statusChip,
  swap,
  switchRow,
} from "./dom.mjs";

const THEMES = [
  ["system", "System"],
  ["light", "Light"],
  ["dark", "Dark"],
];

// One section of Settings: its title and what it is for on the left, its controls on the right.
function section({ id, title, description, headingActions = [], children = [], actions = [] }) {
  return el("section", {
    className: "settings-section",
    attrs: { id, "aria-labelledby": `${id}-title` },
    children: [
      el("div", {
        className: "settings-section-heading",
        children: [
          el("h3", { text: title, attrs: { id: `${id}-title` } }),
          description ? el("p", { text: description }) : null,
          headingActions.length
            ? el("div", { className: "row-actions", children: headingActions })
            : null,
        ],
      }),
      el("div", {
        className: "settings-section-body",
        children: [
          ...children,
          actions.length
            ? el("div", {
                className: "card-actions",
                attrs: { "data-align": "start" },
                children: actions,
              })
            : null,
        ].filter(Boolean),
      }),
    ],
  });
}

// Settings, in the order people look for them: how this computer shares, its own features, then
// updates, startup, the notch, appearance, diagnostics and about.
export function renderSettings(nodes, ctx) {
  clear(nodes.settingsContent);
  const settings = ctx.studio.view === "settings";
  const sections = [
    ["settings-alert", settings ? ctx.pageAlert : null],
    ["settings-sharing", sharingCard(ctx)],
    ["settings-clipboard", clipboardCard(ctx)],
    ["settings-dimming", dimmingCard(ctx)],
    ["settings-pointer", pointerCard(ctx)],
    ["settings-keyboard", keyboardCard(ctx)],
    ["settings-updates", updatesCard(ctx)],
    ["settings-startup", startupCard(ctx)],
    ["settings-island", islandCard(ctx)],
    ["settings-appearance", appearanceCard(ctx)],
    ["settings-diagnostics", diagnosticsCard(ctx)],
    ["settings-about", aboutCard(ctx)],
  ];
  for (const [key, node] of sections) {
    const panel = presence(key, node);
    if (panel) nodes.settingsContent.append(panel);
  }
}

// The network MonHop uses, pausing every computer at once, and the way out that always works.
function sharingCard(ctx) {
  const { actions, busy, toolbar } = ctx;
  const resume = toolbar.pause === "resume";
  return section({
    id: "settings-sharing",
    title: "Sharing",
    description: "How this computer reaches the others.",
    children: [
      networkCard(ctx),
      rows([
        row({
          title: resume ? "Sharing is paused" : "Pause sharing",
          detail: resume
            ? "Resume picks up with the computer you used last."
            : "Stops sharing with every computer. Input stays local.",
          actions: [
            button(resume ? "Resume" : "Pause sharing", {
              variant: "secondary",
              size: "sm",
              disabled: busy || toolbar.pause === null,
              focusKey: "settings-pause",
              onClick: resume ? actions.resumeSharing : actions.pauseSharing,
            }),
          ],
        }),
        row({
          title: "Take your input back",
          detail:
            "Hold both Control keys and Escape for two seconds to take your keyboard and mouse back.",
          actions: [keycaps(ctx.platform)],
        }),
      ]),
    ],
  });
}

// The emergency stop's keys, as this computer's keyboard labels them.
function keycaps(platform) {
  const keys = platform === "macos" ? ["control", "control", "esc"] : ["Ctrl", "Ctrl", "Esc"];
  return el("span", {
    className: "keycaps",
    attrs: { "aria-hidden": "true" },
    children: keys.map((key) => el("kbd", { className: "keycap", text: key })),
  });
}

// Hidden until app.js has heard from clipboard_status or a "clipboard" event, so a backend
// built before clipboard sharing leaves it out rather than showing a broken switch.
function clipboardCard(ctx) {
  const { clipboard, computers, actions } = ctx;
  if (!clipboard) return null;
  const view = normalizeClipboardView(clipboard.view);
  const children = [
    rows([
      switchRow("Share the clipboard", {
        checked: view.enabled,
        description: CLIPBOARD_PRIVACY,
        disabled: clipboard.pending || !ctx.core,
        focusKey: "settings-clipboard-enable",
        onChange: actions.setClipboardEnabled,
      }),
    ]),
  ];
  const status = clipboardStatusText(view);
  const waiting = presence("settings-clipboard-status", status ? note(status) : null);
  if (waiting) children.push(waiting);
  for (const line of clipboardPeerLines(view, computers)) children.push(note(line));
  const accessNotice = clipboardAccessNotice(view);
  if (accessNotice) children.push(note(accessNotice, "danger"));
  const skipNotice = clipboardNoticeText(view);
  if (skipNotice) children.push(note(skipNotice));
  const lastText = lastTransferText(view);
  if (lastText) children.push(note(lastText));
  const pill = clipboardPill(view);
  const headingActions = [
    swap(
      "settings-clipboard-pill",
      pill ? statusChip(pill) : el("span", { className: "chip-slot" }),
      pill ? pill.tone : "none",
    ),
  ];
  return section({
    id: "settings-clipboard",
    title: "Clipboard",
    description: "Text and images you copy, on the computers you share with.",
    headingActions,
    children,
  });
}

// Works without a paired computer: dimming is this computer's own feature.
function dimmingCard({ core, dimming, actions }) {
  const view = dimming.view;
  const disabled = !core || dimming.pending;
  const message = dimmingMessage(dimming);
  return section({
    id: "settings-dimming",
    title: "Screen dimming",
    description:
      "Darkens every display of this computer, above everything, until you toggle it again.",
    children: [
      rows([
        switchRow("Shortcut", {
          checked: view?.enabled ?? true,
          description: shortcutDescription(view),
          disabled,
          focusKey: "settings-dimming-shortcut",
          onChange: actions.setDimmingEnabled,
        }),
        sliderRow("Darkness", {
          value: view?.level ?? 50,
          min: view?.minLevel ?? 10,
          max: view?.maxLevel ?? 99,
          step: DIM_LEVEL_STEP,
          format: levelLabel,
          description: "Changes live while the screen is dimmed.",
          disabled,
          focusKey: "settings-dimming-level",
          onInput: actions.previewDimLevel,
          onChange: actions.setDimLevel,
          onDragStart: actions.beginDimDrag,
          onDragEnd: actions.endDimDrag,
        }),
      ]),
      presence("settings-dimming-message", message ? note(message, "danger") : null),
      el("div", {
        className: "card-actions",
        attrs: { "data-align": "start" },
        children: [
          button(dimButtonLabel(view), {
            variant: "secondary",
            size: "sm",
            iconName: view?.dimmed ? "sun" : "moon",
            disabled,
            busy: dimming.pending,
            focusKey: "settings-dimming-toggle",
            onClick: actions.toggleDimming,
          }),
        ],
      }),
    ],
  });
}

// Only on a Mac, once app.js has Rust's views: autoscroll matters when a Windows mouse controls this
// Mac, swiping between pages when this Mac's trackpad controls another computer.
function pointerCard({ core, autoscroll, swipe, actions }) {
  const switches = [
    autoscroll &&
      switchRow(AUTOSCROLL_LABEL, {
        checked: autoscroll.view.enabled,
        description: AUTOSCROLL_NOTE,
        disabled: !core || autoscroll.pending,
        focusKey: "settings-autoscroll-enable",
        onChange: actions.setAutoscrollEnabled,
      }),
    swipe &&
      switchRow(SWIPE_LABEL, {
        checked: swipe.view.enabled,
        description: SWIPE_NOTE,
        disabled: !core || swipe.pending,
        focusKey: "settings-swipe-enable",
        onChange: actions.setSwipeEnabled,
      }),
  ].filter(Boolean);
  if (!switches.length) return null;
  return section({
    id: "settings-pointer",
    title: "Mouse and trackpad",
    description: "When another computer's mouse controls this Mac, or this trackpad controls it.",
    children: [
      rows(switches),
      presence(
        "settings-autoscroll-message",
        autoscroll?.view.error ? note(autoscroll.view.error, "danger") : null,
      ),
      presence(
        "settings-swipe-message",
        swipe?.view.error ? note(swipe.view.error, "danger") : null,
      ),
    ],
  });
}

// Only on Windows, once app.js has Rust's view: the switch matters when this keyboard controls a Mac.
function keyboardCard({ core, controlAsCommand, actions }) {
  if (!controlAsCommand) return null;
  const { view, pending } = controlAsCommand;
  return section({
    id: "settings-keyboard",
    title: "Keyboard",
    description: "When this keyboard controls a Mac.",
    children: [
      rows([
        switchRow(CONTROL_AS_COMMAND_LABEL, {
          checked: view.enabled,
          description: CONTROL_AS_COMMAND_NOTE,
          disabled: !core || pending,
          focusKey: "settings-control-as-command-enable",
          onChange: actions.setControlAsCommand,
        }),
      ]),
      presence("settings-keyboard-message", view.error ? note(view.error, "danger") : null),
    ],
  });
}

function updatesCard(ctx) {
  const { core, updates, actions } = ctx;
  const { view, pending } = updates;
  const hint = installHint(view);
  return section({
    id: "settings-updates",
    title: "Updates",
    description: "Signed updates from the MonHop releases page.",
    children: [
      rows([
        switchRow("Install updates automatically", {
          checked: view.automatic,
          description:
            "Off until you enable it. Checks github.com and downloads signed updates only while sharing is paused. " +
            "Install by quitting MonHop or pressing Restart to update.",
          disabled: pending || !core,
          id: "settings-updates-automatic",
          onChange: actions.setUpdatesAutomatic,
        }),
      ]),
      note(updatesStatusText(view)),
      // Both come and go with the phase; each glides its own height so the sections below never jump.
      presence(
        "settings-updates-progress",
        view.phase === "downloading"
          ? progressBar("settings-updates-progress", (view.progressPercent ?? 0) / 100)
          : null,
      ),
      presence("settings-updates-hint", hint ? note(hint, "danger") : null),
    ],
    actions: [
      button("Check now", {
        id: "settings-updates-check",
        variant: "secondary",
        size: "sm",
        disabled: pending || !canCheck(view),
        busy: pending,
        onClick: actions.checkForUpdates,
      }),
      button("Restart to update", {
        id: "settings-updates-install",
        size: "sm",
        disabled: pending || !canInstall(view),
        busy: pending,
        onClick: actions.installUpdate,
      }),
    ],
  });
}

// The only place MonHop's start at login is set.
function startupCard(ctx) {
  const { core, platform, autostart, actions } = ctx;
  const { view, pending } = autostart;
  return section({
    id: "settings-startup",
    title: "Startup",
    children: [
      rows([
        switchRow("Start MonHop when you log in", {
          checked: view.enabled,
          description: autostartDescription(platform),
          disabled: pending || !core,
          id: "settings-autostart",
          onChange: actions.setAutostart,
        }),
      ]),
      note(autostartStatusText(view)),
      presence(
        "settings-autostart-open",
        showAutostartOpenSettings(view)
          ? el("div", {
              className: "card-actions",
              attrs: { "data-align": "start" },
              children: [
                button(autostartOpenLabel(platform), {
                  id: "settings-autostart-open",
                  variant: "secondary",
                  size: "sm",
                  disabled: pending,
                  busy: pending,
                  onClick: actions.openAutostartSettings,
                }),
              ],
            })
          : null,
      ),
    ],
  });
}

// Notched Macs only: the context is undefined wherever no display can show the island.
function islandCard(ctx) {
  const { core, island, actions } = ctx;
  if (!island) return null;
  const { view, pending } = island;
  return section({
    id: "settings-island",
    title: ISLAND_TITLE,
    children: [
      rows([
        switchRow(ISLAND_SWITCH_LABEL, {
          checked: view.enabled,
          description: ISLAND_DESCRIPTION,
          disabled: pending || !core,
          id: "settings-island-enabled",
          onChange: actions.setIslandEnabled,
        }),
      ]),
      presence("settings-island-error", view.error ? note(view.error, "danger") : null),
    ],
  });
}

function appearanceCard({ theme, themePending, actions }) {
  const control = el("div", {
    className: "segmented theme-choice",
    attrs: { role: "radiogroup", "aria-label": "Appearance" },
    children: THEMES.map(([value, label]) => {
      const option = el("button", {
        className: "segmented-option",
        text: label,
        attrs: { type: "button", role: "radio", "aria-checked": String(theme === value) },
        dataset: { focusKey: `settings-theme-${value}` },
      });
      option.disabled = themePending;
      option.addEventListener("click", () => actions.setTheme(value));
      return option;
    }),
  });
  return section({
    id: "settings-appearance",
    title: "Appearance",
    description: "System follows this computer's light or dark setting.",
    children: [control],
  });
}

function diagnosticsCard({ state, actions }) {
  const logPath = state.snapshot?.logPath;
  return section({
    id: "settings-diagnostics",
    title: "Diagnostics",
    children: [
      rows([
        row({
          title: "Log file",
          detail: logPath || "Available after this computer is checked.",
          actions: [
            button("Show", {
              variant: "secondary",
              size: "sm",
              disabled: !logPath,
              focusKey: "settings-log-show",
              onClick: actions.revealLogFile,
            }),
          ],
        }),
      ]),
    ],
  });
}

function aboutCard(ctx) {
  const { view } = ctx.updates;
  const { actions } = ctx;
  return section({
    id: "settings-about",
    title: "About",
    children: [
      el("p", {
        className: "about-version",
        text: `MonHop ${view.currentVersion} (${view.buildCommit})`,
      }),
      note("Free software under the GNU GPL v3 or later."),
      el("div", {
        className: "card-actions",
        attrs: { "data-align": "start" },
        children: [
          button("Source code", {
            id: "settings-about-source",
            variant: "secondary",
            size: "sm",
            onClick: () => actions.openLink("source"),
          }),
          button("Release notes", {
            id: "settings-about-releases",
            variant: "secondary",
            size: "sm",
            onClick: () => actions.openLink("releases"),
          }),
          button("Support MonHop", {
            id: "settings-about-support",
            size: "sm",
            iconName: "heart",
            onClick: () => actions.openLink("support"),
          }),
        ],
      }),
    ],
  });
}
