import {
  UPDATE_READY_NOTE,
  canCheck,
  canInstall,
  checkHint,
  installHint,
  updatesStatusText,
} from "./updates-model.mjs";
import {
  autostartDescription,
  autostartOpenLabel,
  autostartStatusText,
  showAutostartOpenSettings,
  trayPlace,
} from "./autostart-model.mjs";
import { ISLAND_DESCRIPTION, ISLAND_SWITCH_LABEL, ISLAND_TITLE } from "./island-model.mjs";
import {
  DIM_LEVEL_STEP,
  dimButtonLabel,
  dimRowTitle,
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
import { ESCAPE_SENTENCE, escapeKeys } from "./escape-model.mjs";
import { fadingText } from "./computer-card.mjs";
import { tweenValue } from "./motion-model.mjs";
import {
  button,
  clear,
  copyAnimation,
  el,
  icon,
  motionEase,
  motionEnabled,
  motionMs,
  note,
  progressBar,
  row,
  rows,
  sinceChanged,
  sliderRow,
  stableLabel,
  statusChip,
  swap,
  switchRow,
  text,
} from "./dom.mjs";

const THEMES = [
  ["system", "System"],
  ["light", "Light"],
  ["dark", "Dark"],
];

// One section of Settings: its title and what it is for on the left, its controls as rows on the right.
function section({ id, title, description, aside = [], children = [] }) {
  return el("section", {
    className: "settings-section",
    attrs: { id, "aria-labelledby": `${id}-title` },
    children: [
      el("div", {
        className: "settings-section-heading",
        children: [
          el("h3", { text: title, attrs: { id: `${id}-title` } }),
          description ? el("p", { text: description }) : null,
          aside.length ? el("div", { className: "row-actions", children: aside }) : null,
        ],
      }),
      el("div", { className: "settings-section-body", children: children.filter(Boolean) }),
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

// A row's words cross-fade in place when they change; the section around it glides to its height.
function fadeCopy(node, key, signature) {
  const copy = node.querySelector(":scope > .row-copy");
  const after = copy.nextSibling;
  node.insertBefore(swap(key, copy, signature, { block: true }), after);
  return node;
}

// The network MonHop uses, pausing every computer at once, and the way out that always works.
function sharingCard(ctx) {
  const { actions, busy, toolbar } = ctx;
  const resume = toolbar.pause === "resume";
  const pause = button("", {
    variant: "secondary",
    size: "sm",
    disabled: busy || toolbar.pause === null || toolbar.pause === "start",
    focusKey: "settings-pause",
    onClick: resume ? actions.resumeSharing : actions.pauseSharing,
  });
  pause.append(stableLabel("settings-pause", ["Pause sharing", "Resume"], resume ? 1 : 0));
  const pauseRow = row({
    title: resume ? "Sharing is paused" : "Pause sharing",
    detail: resume
      ? "Resume turns your computers back on."
      : "Stops sharing with every computer. Input stays local.",
    actions: [pause],
  });
  return section({
    id: "settings-sharing",
    title: "Sharing",
    description: "How this computer reaches the others.",
    children: [
      networkCard(ctx),
      rows([
        fadeCopy(pauseRow, "settings-pause-copy", resume ? "resume" : "pause"),
        row({
          title: "Take your input back",
          detail: ESCAPE_SENTENCE,
          actions: [keycaps(ctx.platform)],
        }),
      ]),
    ],
  });
}

// The emergency stop's keys, as this computer's keyboard labels them.
function keycaps(platform) {
  return el("span", {
    className: "keycaps",
    attrs: { "aria-hidden": "true" },
    children: escapeKeys(platform).map((key) => el("kbd", { className: "keycap", text: key })),
  });
}

// Hidden until app.js has heard from clipboard_status or a "clipboard" event, so a backend
// built before clipboard sharing leaves it out rather than showing a broken switch.
function clipboardCard(ctx) {
  const { clipboard, computers, actions } = ctx;
  if (!clipboard) return null;
  const view = normalizeClipboardView(clipboard.view);
  const peers = clipboardPeerLines(view, computers);
  const pill = clipboardPill(view);
  return section({
    id: "settings-clipboard",
    title: "Clipboard",
    description: "Text and images you copy, on the computers you share with.",
    aside: [
      swap(
        "settings-clipboard-pill",
        pill ? statusChip(pill) : el("span", { className: "chip-slot" }),
        pill ? pill.tone : "none",
      ),
    ],
    children: [
      rows([
        switchRow("Share the clipboard", {
          checked: view.enabled,
          description: CLIPBOARD_PRIVACY,
          disabled: clipboard.pending || !ctx.core,
          focusKey: "settings-clipboard-enable",
          onChange: actions.setClipboardEnabled,
        }),
      ]),
      fadingText("settings-clipboard-status", "note", clipboardStatusText(view)),
      presence(
        "settings-clipboard-peers",
        peers.length
          ? el("div", { className: "settings-notes", children: peers.map((line) => note(line)) })
          : null,
      ),
      fadingText("settings-clipboard-access", "note", clipboardAccessNotice(view), "danger"),
      fadingText("settings-clipboard-skip", "note", clipboardNoticeText(view)),
      fadingText("settings-clipboard-last", "note", lastTransferText(view)),
    ],
  });
}

// Works without a paired computer: dimming is this computer's own feature.
function dimmingCard({ core, dimming, actions }) {
  const view = dimming.view;
  const disabled = !core || dimming.pending;
  const dimmed = view?.dimmed === true;
  const toggle = button("", {
    variant: "secondary",
    size: "sm",
    disabled,
    busy: dimming.pending,
    focusKey: "settings-dimming-toggle",
    onClick: actions.toggleDimming,
  });
  toggle.append(
    swap("settings-dimming-glyph", icon(dimmed ? "sun" : "moon"), dimmed ? "sun" : "moon"),
    stableLabel(
      "settings-dimming-toggle",
      [dimButtonLabel({ dimmed: false }), dimButtonLabel({ dimmed: true })],
      dimmed ? 1 : 0,
    ),
  );
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
        fadeCopy(
          row({ title: dimRowTitle(view), actions: [toggle] }),
          "settings-dimming-copy",
          dimmed,
        ),
      ]),
      fadingText("settings-dimming-message", "note", dimmingMessage(dimming), "danger"),
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
    description:
      "When another computer's mouse controls this Mac, or this Mac's trackpad controls another computer.",
    children: [
      rows(switches),
      fadingText("settings-autoscroll-message", "note", autoscroll?.view.error, "danger"),
      fadingText("settings-swipe-message", "note", swipe?.view.error, "danger"),
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
      fadingText("settings-keyboard-message", "note", view.error, "danger"),
    ],
  });
}

// The status row offers Check now, or Restart to update once a build is ready. Both share one
// cell, so the row keeps its shape and the swap is a cross-fade.
function updatesCard(ctx) {
  const { core, updates, actions } = ctx;
  const { view, pending } = updates;
  const ready = view.phase === "ready";
  const check = button("Check now", {
    id: "settings-updates-check",
    variant: "secondary",
    size: "sm",
    disabled: pending || !canCheck(view),
    busy: pending,
    onClick: actions.checkForUpdates,
  });
  const install = button("Restart to update", {
    id: "settings-updates-install",
    size: "sm",
    disabled: pending || !canInstall(view),
    busy: pending,
    onClick: actions.installUpdate,
  });
  const status = updatesStatusText(view);
  const statusRow = row({
    title: status,
    detail: ready ? UPDATE_READY_NOTE : null,
    actions: [oneOf("settings-updates-action", [check, install], ready ? 1 : 0)],
  });
  statusRow.classList.add("settings-status-row");
  // Comes and goes with the phase, gliding its own height so the sections below never jump.
  const progress = presence(
    "settings-updates-progress",
    view.phase === "downloading"
      ? progressBar("settings-updates-progress", (view.progressPercent ?? 0) / 100)
      : null,
  );
  if (progress) statusRow.querySelector(":scope > .row-copy").append(progress);
  return section({
    id: "settings-updates",
    title: "Updates",
    description: "Signed updates from the MonHop releases page.",
    children: [
      rows([
        switchRow("Install updates automatically", {
          checked: view.automatic,
          description:
            "Checks github.com and downloads signed updates only while sharing is paused. " +
            "Install by quitting MonHop or pressing Restart to update.",
          disabled: pending || !core,
          id: "settings-updates-automatic",
          onChange: actions.setUpdatesAutomatic,
        }),
        // Keyed by phase, so a download's rising percentage counts up in place.
        fadeCopy(statusRow, "settings-updates-copy", view.phase),
      ]),
      fadingText("settings-updates-hint", "note", installHint(view) || checkHint(view)),
    ],
  });
}

// Controls that take turns in one place: every one shares a cell, so the widest sets the width and
// nothing beside them moves; only `current` is shown and reachable, and a change cross-fades.
function oneOf(key, controls, current) {
  const cell = stableLabel(
    key,
    controls.map(() => ""),
    current,
  );
  cell.classList.add("one-of");
  cell.replaceChildren(
    ...controls.map((control, index) => {
      const shown = index === current;
      control.dataset.current = String(shown);
      control.inert = !shown;
      if (!shown) control.setAttribute("aria-hidden", "true");
      return control;
    }),
  );
  return cell;
}

// The only place MonHop's start at login is set.
function startupCard(ctx) {
  const { core, platform, autostart, actions } = ctx;
  const { view, pending } = autostart;
  return section({
    id: "settings-startup",
    title: "Startup",
    description: `Closing the window keeps MonHop running in ${trayPlace(platform)}.`,
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
      fadingText("settings-autostart-status", "note", autostartStatusText(view)),
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
    description: "Only on a Mac whose display has a camera notch.",
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
      fadingText("settings-island-error", "note", view.error, "danger"),
    ],
  });
}

function appearanceCard(ctx) {
  return section({
    id: "settings-appearance",
    title: "Appearance",
    description: "System follows this computer's light or dark setting.",
    children: [rows([row({ title: "Theme", actions: [themeChoice(ctx)] })])],
  });
}

// The theme choice's thumb, by key: a render rebuilds the control, so the new thumb glides on from
// where the last one was drawn, or carries a glide still running toward the same theme.
const thumbGlides = new Map();
// The theme the thumb is on and the one it left, whose words fade back as the new one's brighten.
const themeWords = { current: null, previous: null };

function themeChoice({ theme, themePending, actions }) {
  const index = Math.max(
    0,
    THEMES.findIndex(([value]) => value === theme),
  );
  const thumb = el("span", { className: "segmented-thumb", attrs: { "aria-hidden": "true" } });
  thumb.style.setProperty("--segment-index", String(index));
  glideThumb(thumb, "settings-theme", index);
  // The words trade emphasis with the thumb, carried on across the renders meanwhile.
  if (themeWords.current !== index) themeWords.previous = themeWords.current;
  themeWords.current = index;
  const elapsed = sinceChanged("settings-theme-words", index);
  const control = el("div", {
    className: "segmented theme-choice",
    attrs: { role: "radiogroup", "aria-label": "Appearance" },
    children: [
      thumb,
      ...THEMES.map(([value, label], at) => {
        const option = el("button", {
          className: "segmented-option",
          text: label,
          attrs: { type: "button", role: "radio", "aria-checked": String(theme === value) },
          dataset: { focusKey: `settings-theme-${value}` },
        });
        option.disabled = themePending;
        option.addEventListener("click", () => actions.setTheme(value));
        const trade = at === index ? "on" : at === themeWords.previous ? "off" : null;
        if (trade && elapsed < motionMs("--motion-base")) {
          option.dataset.enter = trade;
          option.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
        }
        return option;
      }),
    ],
  });
  control.style.setProperty("--segment-count", String(THEMES.length));
  return control;
}

// Each step of the thumb is one segment plus the gap after it, which is the thumb's own width.
function segmentAt(index) {
  return `translateX(${index * 100}%)`;
}

function glideThumb(thumb, key, index) {
  const last = thumbGlides.get(key) ?? null;
  const running = last?.animation?.playState === "running" ? last : null;
  const shown = running
    ? tweenValue(running.from, running.to, running.animation.effect.getComputedTiming().progress)
    : (last?.to ?? index);
  if (motionEnabled() && running?.to === index) {
    thumbGlides.set(key, { ...running, animation: copyAnimation(running.animation, thumb) });
    return;
  }
  if (!motionEnabled() || shown === index) {
    thumbGlides.set(key, { animation: null, from: index, to: index });
    return;
  }
  const animation = thumb.animate(
    { transform: [segmentAt(shown), segmentAt(index)] },
    { duration: motionMs("--motion-spring"), easing: motionEase("--ease-spring") },
  );
  thumbGlides.set(key, { animation, from: shown, to: index });
}

function diagnosticsCard({ state, actions }) {
  const logPath = state.snapshot?.logPath;
  const log = row({
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
  });
  if (logPath) log.classList.add("settings-path-row");
  return section({
    id: "settings-diagnostics",
    title: "Diagnostics",
    description: "Kept on this computer. MonHop never logs what you type.",
    children: [rows([log])],
  });
}

function aboutCard(ctx) {
  const { view } = ctx.updates;
  const { actions } = ctx;
  const link = (label, target, id, iconName) =>
    button(label, {
      id,
      variant: "ghost",
      size: "sm",
      iconName,
      onClick: () => actions.openLink(target),
    });
  return section({
    id: "settings-about",
    title: "About",
    children: [
      el("div", {
        className: "settings-about",
        children: [
          el("p", {
            className: "about-line",
            children: [
              el("strong", { text: `MonHop ${view.currentVersion}` }),
              view.buildCommit ? text(" ") : null,
              view.buildCommit
                ? el("code", { className: "about-commit", text: `(${view.buildCommit})` })
                : null,
            ],
          }),
          note("Free software under the GNU GPL v3 or later."),
          el("div", {
            className: "settings-links",
            children: [
              link("Source code", "source", "settings-about-source"),
              link("Release notes", "releases", "settings-about-releases"),
              link("Support MonHop", "support", "settings-about-support", "heart"),
            ],
          }),
        ],
      }),
    ],
  });
}
