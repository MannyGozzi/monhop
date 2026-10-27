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
import {
  ISLAND_CLIPBOARD_DESCRIPTION,
  ISLAND_CLIPBOARD_LABEL,
  ISLAND_DESCRIPTION,
  ISLAND_SWITCH_LABEL,
  ISLAND_TITLE,
} from "./island-model.mjs";
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
import { glideMove, presence } from "./accordion.mjs";
import { listStep } from "./studio-model.mjs";
import { networkCard } from "./screen-ready.mjs";
import { ESCAPE_SENTENCE, escapeKeys } from "./escape-model.mjs";
import { fadingText } from "./computer-card.mjs";
import { tweenValue } from "./motion-model.mjs";
import {
  button,
  copyAnimation,
  el,
  glideIndicator,
  icon,
  motionEase,
  motionEnabled,
  motionMs,
  note,
  progressBar,
  row,
  rows,
  settled,
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

// The pages of Settings in rail order: what MonHop does across computers and on this one, then the
// app itself. A page this computer has nothing for stays out of the rail.
const PAGES = [
  { key: "sharing", title: "Sharing", icon: "link", build: sharingPage },
  {
    key: "clipboard",
    title: "Clipboard",
    icon: "clipboard",
    available: (ctx) => Boolean(ctx.clipboard),
    build: clipboardPage,
  },
  { key: "dimming", title: "Screen dimming", icon: "moon", build: dimmingPage },
  {
    key: "pointer",
    title: "Mouse and trackpad",
    icon: "mouse",
    available: (ctx) => Boolean(ctx.autoscroll || ctx.swipe),
    build: pointerPage,
  },
  {
    key: "keyboard",
    title: "Keyboard",
    icon: "keyboard",
    available: (ctx) => Boolean(ctx.controlAsCommand),
    build: keyboardPage,
  },
  { key: "general", title: "General", icon: "settings", cluster: true, build: generalPage },
  { key: "updates", title: "Updates", icon: "download", build: updatesPage },
  { key: "about", title: "About", icon: null, build: aboutPage },
];

// The rail outlives renders, so its indicator glides and hover carries on; the page beside it is
// rebuilt each render and cross-fades when another one is chosen.
let shownPage = PAGES[0].key;
let builtPage = null;
let frame = null;
let lastRender = null;
// Pages fading out from a scrolled position, each drawn where it was until its exit ends.
let leavingOffsets = [];
// When Settings last closed. A closed pane is still laid out, so once its exit has played the page
// is not rebuilt until it opens again, settled.
let closedAt = -Infinity;

// Settings opens on this page next; a link elsewhere names the page its control lives on.
export function showSettingsPage(key) {
  if (PAGES.some((page) => page.key === key)) shownPage = key;
}

export function renderSettings(nodes, ctx) {
  lastRender = { nodes, ctx };
  if (ctx.studio.view === "settings") closedAt = null;
  else {
    closedAt ??= performance.now();
    if (performance.now() - closedAt >= motionMs("--motion-base")) {
      builtPage = null;
      return;
    }
  }
  if (!frame) frame = mountFrame(nodes.settingsContent);
  const pages = PAGES.filter((page) => page.available?.(ctx) ?? true);
  if (!pages.some((page) => page.key === shownPage)) shownPage = pages[0].key;
  placeRail(pages);
  const page = pages.find((item) => item.key === shownPage);
  const alert = presence("settings-alert", ctx.studio.view === "settings" ? ctx.pageAlert : null);
  // A page coming back builds settled, so what changed while it was hidden does not replay.
  const fresh = builtPage !== page.key;
  builtPage = page.key;
  const node = fresh ? settled(() => pageNode(page, ctx)) : pageNode(page, ctx);
  const shown = swap("settings-page", node, page.key, { block: true, scale: false });
  frame.column.replaceChildren(...[alert, shown].filter(Boolean));
  holdLeavingPages();
  for (const [key, item] of frame.items)
    item.setAttribute("aria-current", key === shownPage ? "page" : "false");
  glideIndicator(frame.indicator, frame.items.get(shownPage));
}

function mountFrame(content) {
  const nav = el("nav", { className: "settings-nav", attrs: { "aria-label": "Settings pages" } });
  nav.addEventListener("keydown", walkRail);
  const column = el("div", { className: "settings-column" });
  content.replaceChildren(nav, column);
  return {
    nav,
    column,
    indicator: el("span", {
      className: "settings-nav-indicator",
      attrs: { "aria-hidden": "true" },
    }),
    items: new Map(),
    order: [],
  };
}

// Only a change to which pages exist redraws the rail: one that joins grows in, one that leaves
// closes where it was.
function placeRail(pages) {
  const keys = pages.map((page) => page.key);
  if (keys.join() === frame.order.join()) return;
  const first = frame.order.length === 0;
  // A presence glides height, so in the strip, where items sit side by side, the ones that stay
  // slide over from where they were drawn instead of jumping.
  const strip = !first && getComputedStyle(frame.nav).flexDirection === "row";
  const drawn = new Map(
    strip ? [...frame.items].map(([key, item]) => [key, item.getBoundingClientRect()]) : [],
  );
  const order = PAGES.filter((page) => keys.includes(page.key) || frame.order.includes(page.key));
  const children = [];
  for (const page of order) {
    if (page.cluster && children.length)
      children.push(
        el("span", { className: "settings-nav-divider", attrs: { "aria-hidden": "true" } }),
      );
    const kept = keys.includes(page.key);
    if (kept && !frame.items.has(page.key)) {
      frame.items.set(page.key, railItem(page));
      if (!first) presence(`settings-nav-${page.key}`, null);
    }
    const panel = presence(`settings-nav-${page.key}`, kept ? frame.items.get(page.key) : null);
    if (!kept) frame.items.delete(page.key);
    if (panel) children.push(panel);
  }
  frame.nav.replaceChildren(...children);
  frame.order = keys;
  // After the presence panels open, which they do in a microtask of their own, so each slide ends
  // where its item lands.
  if (drawn.size)
    queueMicrotask(() => {
      for (const [key, from] of drawn)
        if (frame.items.has(key)) glideMove(frame.items.get(key), from);
    });
}

function railItem(page) {
  const item = el("button", {
    className: "settings-nav-item",
    attrs: { type: "button" },
    dataset: { focusKey: `settings-nav-${page.key}` },
    children: [pageTile(page), el("span", { className: "settings-nav-label", text: page.title })],
  });
  item.addEventListener("click", () => choosePage(page.key));
  return item;
}

// The tile a page is known by, in the rail and large beside its title.
function pageTile(page, size = "sm") {
  return el("span", {
    className: "settings-tile",
    dataset: { page: page.key, size },
    attrs: { "aria-hidden": "true" },
    children: [page.icon ? icon(page.icon) : el("span", { className: "app-mark" })],
  });
}

// Choosing a page re-renders Settings alone from the last render's state, which stays current until
// the next one. The page that left keeps the place it was scrolled to while it fades.
function choosePage(key) {
  if (key === shownPage || !lastRender) return;
  const scroll = lastRender.nodes.settingsScroll;
  const scrolled = scroll.scrollTop;
  if (scrolled && motionEnabled())
    leavingOffsets.push({
      page: shownPage,
      offset: scrolled,
      until: performance.now() + motionMs("--motion-fast"),
    });
  shownPage = key;
  renderSettings(lastRender.nodes, lastRender.ctx);
  scroll.scrollTop = 0;
  holdLeavingPages();
  revealItem(frame.items.get(key));
}

// Every render rebuilds the fading copies, so each one is put back where it was drawn.
function holdLeavingPages() {
  const now = performance.now();
  leavingOffsets = leavingOffsets.filter((leaving) => leaving.until > now);
  for (const { page, offset } of leavingOffsets)
    for (const ghost of frame.column.querySelectorAll(
      `.settings-page[data-exit][data-page="${page}"]`,
    ))
      ghost.style.marginTop = `${-offset}px`;
}

// While the rail runs across a narrow pane, the chosen page's item scrolls fully into view.
function revealItem(item) {
  const { nav } = frame;
  if (!item || nav.scrollWidth <= nav.clientWidth) return;
  const inset = Number.parseFloat(getComputedStyle(nav).paddingLeft) || 0;
  const box = nav.getBoundingClientRect();
  const at = item.getBoundingClientRect();
  const left = Math.min(at.left - box.left - inset, 0) || Math.max(at.right - box.right + inset, 0);
  if (left) nav.scrollBy({ left, behavior: motionEnabled() ? "smooth" : "auto" });
}

// Up and Down (Left and Right while the rail runs across the top), Home and End walk the pages.
function walkRail(event) {
  if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
  const items = [
    ...frame.nav.querySelectorAll(".settings-nav-item:not([data-exit], [data-exit] *)"),
  ];
  const key = { ArrowLeft: "ArrowUp", ArrowRight: "ArrowDown" }[event.key] ?? event.key;
  const index = listStep(
    items.length,
    items.indexOf(event.target.closest(".settings-nav-item")),
    key,
  );
  if (index < 0) return;
  event.preventDefault();
  items[index].focus({ preventScroll: true });
  revealItem(items[index]);
}

// A page: its tile, title and what it is for, then its groups. `heading` replaces the title's words.
function pageNode(page, ctx) {
  const { heading, description, aside = null, groups } = page.build(ctx);
  return el("section", {
    className: "settings-page",
    dataset: { page: page.key },
    attrs: { "aria-labelledby": "settings-page-title" },
    children: [
      el("header", {
        className: "settings-page-header",
        children: [
          pageTile(page, "lg"),
          el("div", {
            className: "settings-page-heading",
            children: [
              el("h2", {
                attrs: { id: "settings-page-title" },
                children: heading ?? [text(page.title)],
              }),
              description ? el("p", { text: description }) : null,
            ],
          }),
          aside,
        ],
      }),
      ...groups,
    ],
  });
}

// A run of rows on a page, under its own title when the page has more than one.
function group({ id, title, description, children = [] }) {
  return el("section", {
    className: "settings-group",
    attrs: { id, "aria-labelledby": title ? `${id}-title` : null },
    children: [
      title
        ? el("div", {
            className: "settings-group-heading",
            children: [
              el("h3", { text: title, attrs: { id: `${id}-title` } }),
              description ? el("p", { text: description }) : null,
            ],
          })
        : null,
      ...children,
    ],
  });
}

// A row's words cross-fade in place when they change; the section around it glides to its height.
function fadeCopy(node, key, signature) {
  const copy = node.querySelector(":scope > .row-copy");
  const after = copy.nextSibling;
  node.insertBefore(swap(key, copy, signature, { block: true }), after);
  return node;
}

// The network MonHop uses, pausing every computer at once, and the way out that always works.
function sharingPage(ctx) {
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
  return {
    description: "How this computer reaches the others.",
    groups: [
      presence("settings-network", networkCard(ctx)),
      group({
        id: "settings-pause",
        children: [rows([fadeCopy(pauseRow, "settings-pause-copy", resume ? "resume" : "pause")])],
      }),
      group({
        id: "settings-escape",
        title: "Take your input back",
        children: [escapeKeysPanel(ctx.platform)],
      }),
    ],
  };
}

// The emergency stop, drawn as the keys to hold as this computer's keyboard labels them.
function escapeKeysPanel(platform) {
  const caps = escapeKeys(platform).flatMap((key, index) => [
    index ? el("span", { className: "settings-escape-plus", text: "+" }) : null,
    el("kbd", { className: "keycap", text: key }),
  ]);
  return el("div", {
    className: "settings-escape",
    children: [
      el("span", {
        className: "settings-escape-keys",
        attrs: { "aria-hidden": "true" },
        children: caps,
      }),
      el("p", { text: ESCAPE_SENTENCE }),
    ],
  });
}

// Hidden until app.js has heard from clipboard_status or a "clipboard" event, so a backend
// built before clipboard sharing leaves it out rather than showing a broken switch.
function clipboardPage(ctx) {
  const { clipboard, computers, actions } = ctx;
  const view = normalizeClipboardView(clipboard.view);
  const peers = clipboardPeerLines(view, computers);
  const pill = clipboardPill(view);
  return {
    description: "Text and images you copy, on the computers you share with.",
    aside: swap(
      "settings-clipboard-pill",
      pill ? statusChip(pill) : el("span", { className: "chip-slot" }),
      pill ? pill.tone : "none",
    ),
    groups: [
      group({
        id: "settings-clipboard",
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
              ? el("div", {
                  className: "settings-notes",
                  children: peers.map((line) => note(line)),
                })
              : null,
          ),
          fadingText("settings-clipboard-access", "note", clipboardAccessNotice(view), "danger"),
          fadingText("settings-clipboard-skip", "note", clipboardNoticeText(view)),
          fadingText("settings-clipboard-last", "note", lastTransferText(view)),
        ],
      }),
    ],
  };
}

// Works without a paired computer: dimming is this computer's own feature.
function dimmingPage({ core, dimming, actions }) {
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
  return {
    description:
      "Darkens every display of this computer, above everything, until you toggle it again.",
    groups: [
      group({
        id: "settings-dimming",
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
      }),
    ],
  };
}

// Only on a Mac, once app.js has Rust's views: autoscroll matters when a Windows mouse controls this
// Mac, swiping between pages when this Mac's trackpad controls another computer.
function pointerPage({ core, autoscroll, swipe, actions }) {
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
  return {
    description:
      "When another computer's mouse controls this Mac, or this Mac's trackpad controls another computer.",
    groups: [
      group({
        id: "settings-pointer",
        children: [
          rows(switches),
          fadingText("settings-autoscroll-message", "note", autoscroll?.view.error, "danger"),
          fadingText("settings-swipe-message", "note", swipe?.view.error, "danger"),
        ],
      }),
    ],
  };
}

// Only on Windows, once app.js has Rust's view: the switch matters when this keyboard controls a Mac.
function keyboardPage({ core, controlAsCommand, actions }) {
  const { view, pending } = controlAsCommand;
  return {
    description: "When this keyboard controls a Mac.",
    groups: [
      group({
        id: "settings-keyboard",
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
      }),
    ],
  };
}

// The status row offers Check now, or Restart to update once a build is ready. Both share one
// cell, so the row keeps its shape and the swap is a cross-fade.
function updatesPage(ctx) {
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
  return {
    description: "Signed updates from the MonHop releases page.",
    groups: [
      group({
        id: "settings-updates",
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
      }),
    ],
  };
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

// How MonHop looks, starts and shows itself beside the notch.
function generalPage(ctx) {
  return {
    description: "How MonHop looks and starts.",
    groups: [
      appearanceGroup(ctx),
      startupGroup(ctx),
      presence("settings-island", islandGroup(ctx)),
    ],
  };
}

// The only place MonHop's start at login is set.
function startupGroup(ctx) {
  const { core, platform, autostart, actions } = ctx;
  const { view, pending } = autostart;
  return group({
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
function islandGroup(ctx) {
  const { core, island, actions } = ctx;
  if (!island) return null;
  const { view, pending } = island;
  return group({
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
        // Copies only show while the island itself does.
        switchRow(ISLAND_CLIPBOARD_LABEL, {
          checked: view.clipboardNotices,
          description: ISLAND_CLIPBOARD_DESCRIPTION,
          disabled: pending || !core || !view.enabled,
          id: "settings-island-clipboard",
          onChange: actions.setIslandClipboardNotices,
        }),
      ]),
      fadingText("settings-island-error", "note", view.error, "danger"),
    ],
  });
}

function appearanceGroup(ctx) {
  return group({
    id: "settings-appearance",
    title: "Appearance",
    children: [
      rows([
        row({
          title: "Theme",
          detail: "System follows this computer's light or dark setting.",
          actions: [themeChoice(ctx)],
        }),
      ]),
    ],
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

// The version and where it comes from, then the log kept on this computer.
function aboutPage(ctx) {
  const { view } = ctx.updates;
  return {
    heading: [
      text(`MonHop ${view.currentVersion}`),
      view.buildCommit ? el("code", { className: "about-commit", text: view.buildCommit }) : null,
    ],
    description: "Free software under the GNU GPL v3 or later.",
    groups: [
      group({
        id: "settings-links",
        children: [
          rows([
            linkRow(ctx, "Source code", "source", "settings-about-source"),
            linkRow(ctx, "Release notes", "releases", "settings-about-releases"),
            linkRow(ctx, "Support MonHop", "support", "settings-about-support", "heart"),
          ]),
        ],
      }),
      diagnosticsGroup(ctx),
    ],
  };
}

// A row that opens one of MonHop's pages in the browser.
function linkRow({ actions }, label, target, id, iconName = null) {
  const node = el("button", {
    className: "row settings-link-row",
    attrs: { type: "button", id },
    children: [
      el("span", {
        className: "row-copy",
        children: [el("strong", { children: [text(label), iconName ? icon(iconName) : null] })],
      }),
      el("span", { className: "row-actions", children: [icon("arrow-up-right")] }),
    ],
  });
  node.addEventListener("click", () => actions.openLink(target));
  return node;
}

function diagnosticsGroup({ state, actions }) {
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
  return group({
    id: "settings-diagnostics",
    title: "Diagnostics",
    description: "Kept on this computer. MonHop never logs what you type.",
    children: [rows([log])],
  });
}
