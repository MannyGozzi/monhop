import { computerStatus, isSessionStatus } from "./computer-status.mjs";
import { displayName, findComputer } from "./computers-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import {
  MAX_ARRANGEMENT_NAME,
  arrangementForSharing,
  arrangementResetTarget,
  canApplySetup,
  canLoadArrangement,
  canResetArrangement,
  canSaveArrangement,
  displayNoticeCopy,
  displayUseChoices,
  groupFromView,
  hasAppliedCurrentLayout,
  hasAppliedLayout,
  isConnected,
  isSyncing,
  normalizeArrangementName,
  sharedMonitorChoices,
} from "./sharing-model.mjs";
import { createArrangementView } from "./arrangement-view.mjs";
import { newestFirst, resolveEnabledList } from "./computer-card-model.mjs";
import { layoutChipStrip } from "./computer-card.mjs";
import { presence } from "./accordion.mjs";
import { createDashboardArrangement, createLocalPicture } from "./dashboard-arrangement.mjs";
import { UPDATE_READY_NOTE, canInstall, installHint } from "./updates-model.mjs";
import {
  badge,
  button,
  clear,
  copyAnimation,
  el,
  icon,
  motionEase,
  motionEnabled,
  motionMs,
  motionToken,
  note,
  row,
  rows,
  setLabel,
  sinceChanged,
  stableLabel,
  stateCard,
} from "./dom.mjs";

// The picture's motion runs on the same tokens and helpers as every other UI motion.
const MOTION = {
  enabled: motionEnabled,
  ms: motionMs,
  ease: motionEase,
  copy: copyAnimation,
  token: motionToken,
};
// A computer in these states cannot be reached, so its displays are drawn offline.
const OFFLINE_KEYS = new Set(["error", "attention"]);
// A first guess at the height of the card under a picture, corrected once the card is laid out.
const CARD_ESTIMATE = 148;
const SLOT_COPY = { title: "New computer", detail: "Its displays appear here" };
const ESCAPE_SENTENCE =
  "Hold both Control keys and Esc for 2 seconds to take your keyboard and mouse back.";

// The view on the canvas, kept across renders so its state changes transition in place; it is
// replaced, cross-fading, only when what it shows changes.
let shown = null;
let stageObserver = null;
let escapePlatform = null;

// The canvas: its banners, the arrangement (read only, or the live editor while arranging), the
// Apply bar and the status bar.
export function renderCanvas(nodes, ctx) {
  watchStage(nodes.canvasStage);
  renderBanners(nodes.canvasBanners, ctx);
  const mode = canvasMode(ctx);
  nodes.canvasPane.dataset.mode = mode;
  if (mode !== "edit") releaseEditor();
  const signature = viewSignature(mode, ctx);
  if (shown?.signature === signature && shown.element.isConnected) shown.update(ctx);
  else {
    const view = buildView(mode, ctx, stageSize(nodes.canvasStage));
    showView(nodes.canvasStage, { ...view, signature });
    view.mounted?.();
  }
  clear(nodes.canvasDock);
  const drawer = presence("canvas-drawer", mode === "edit" ? drawerContent(ctx) : null);
  if (drawer) nodes.canvasDock.append(drawer);
  const bar = presence("apply-bar", ctx.arrange.active ? applyBar(ctx, mode) : null);
  if (bar) nodes.canvasDock.append(bar);
  renderStatusBar(nodes, ctx);
}

function canvasMode(ctx) {
  const { arrange, sharing, studio, computers } = ctx;
  if (arrange.active)
    return isConnected(sharing) && hasEditableArrangement(sharing) ? "edit" : "waiting";
  if (!computers.items.length) return "empty";
  return studio.canvasComputer?.setup?.saved === true ? "saved" : "unarranged";
}

function hasEditableArrangement(sharing) {
  const arrangement = arrangementForSharing(sharing);
  return Boolean(arrangement?.groups?.local && arrangement.groups?.peer);
}

// Each computer's picture is its own view, so moving between computers cross-fades.
function viewSignature(mode, ctx) {
  const fingerprint = ctx.studio.canvasComputer?.fingerprint ?? "";
  if (mode === "waiting") return `${mode}:${isConnected(ctx.sharing)}`;
  return mode === "saved" || mode === "unarranged" ? `${mode}:${fingerprint}` : mode;
}

function buildView(mode, ctx, stage) {
  if (mode === "edit") return editView(ctx);
  if (mode === "saved") return savedView(ctx, stage);
  if (mode === "waiting") return waitingView(ctx, stage);
  if (mode === "unarranged") return unarrangedView(ctx, stage);
  return emptyView(ctx, stage);
}

function stageSize(node) {
  return { width: node.clientWidth, height: node.clientHeight };
}

// One observer for the life of the window: the canvas refits its picture whenever its own box
// changes, whether the window, a banner or the inspector moved it.
function watchStage(stage) {
  if (stageObserver || typeof ResizeObserver !== "function") return;
  stageObserver = new ResizeObserver(() => shown?.refit?.(stageSize(stage)));
  stageObserver.observe(stage);
}

// The outgoing view fades where it stands while the new one rises in; a view replaced mid-fade
// leaves from the opacity it had reached.
function showView(stage, view) {
  const leaving = shown?.element?.isConnected ? shown.element : null;
  if (leaving) {
    const opacity = Number(getComputedStyle(leaving).opacity);
    for (const animation of leaving.getAnimations()) animation.cancel();
    leaving.dataset.exit = "true";
    leaving.inert = true;
    leaving.setAttribute("aria-hidden", "true");
    for (const node of [leaving, ...leaving.querySelectorAll("[id], [data-focus-key]")]) {
      node.removeAttribute("id");
      delete node.dataset.focusKey;
    }
    if (motionEnabled() && opacity > 0)
      leaving
        .animate([{ opacity }, { opacity: 0 }], {
          duration: motionMs("--motion-fast"),
          easing: motionEase(),
          fill: "forwards",
        })
        .finished.then(
          () => leaving.remove(),
          () => leaving.remove(),
        );
    else leaving.remove();
  }
  for (const stale of stage.querySelectorAll(":scope > :not([data-exit='true'])")) stale.remove();
  stage.append(view.element);
  shown = view;
  if (leaving && motionEnabled())
    view.element.animate(
      [
        {
          opacity: 0,
          transform: `translateY(${motionToken("--motion-rise")}) scale(${motionToken("--scale-sheet")})`,
        },
        { opacity: 1, transform: "none" },
      ],
      { duration: motionMs("--motion-base"), easing: motionEase() },
    );
}

// --- banners ------------------------------------------------------------------

function renderBanners(container, ctx) {
  clear(container);
  const canvas = ctx.studio.view === "canvas";
  const banners = [
    ["canvas-alert", canvas ? ctx.pageAlert : null],
    ...attentionBanners(ctx),
    ["canvas-display-notice", displayNoticeBanner(ctx)],
    ["canvas-update-ready", updateReadyBanner(ctx)],
  ];
  for (const [key, node] of banners) {
    const panel = presence(key, canvas ? node : null);
    if (panel) container.append(panel);
  }
}

export function banner({ tone, iconName, title, body, actions = [], id }) {
  return el("section", {
    className: "banner",
    dataset: { tone },
    attrs: { id, role: tone === "danger" ? "alert" : "status" },
    children: [
      el("span", {
        className: "banner-glyph",
        attrs: { "aria-hidden": "true" },
        children: [icon(iconName, 16)],
      }),
      el("div", {
        className: "banner-copy",
        children: [
          title ? el("strong", { text: title }) : null,
          body ? el("p", { text: body, attrs: { title: body } }) : null,
        ],
      }),
      actions.length ? el("div", { className: "banner-actions", children: actions }) : null,
    ],
  });
}

// A computer switched on that cannot connect says so above the canvas, with a way to retry or let go.
function attentionBanners(ctx) {
  const { computers, sharing, active, actions, busy } = ctx;
  const local = localName(ctx);
  const enabled = resolveEnabledList(sharing.view, computers, active);
  const known = new Set(enabled);
  const shownBanners = [];
  for (const computer of computers.items) {
    const key = `canvas-attention-${computer.fingerprint}`;
    const status = known.has(computer.fingerprint)
      ? computerStatus(computer, sharing.view, active, local)
      : null;
    const attention = status && OFFLINE_KEYS.has(status.key);
    shownBanners.push([
      key,
      attention
        ? banner({
            tone: status.key === "error" ? "danger" : "warning",
            iconName: "triangle-alert",
            title: `${displayName(computer)}: ${status.label}`,
            body: status.detail,
            actions: [
              button("Pause", {
                variant: "ghost",
                size: "sm",
                disabled: busy,
                focusKey: `${key}-pause`,
                onClick: () => actions.setComputerEnabled(computer.fingerprint, false),
              }),
              button("Try again", {
                variant: "secondary",
                size: "sm",
                iconName: "refresh-cw",
                disabled: busy,
                focusKey: `${key}-retry`,
                onClick: () => actions.setComputerEnabled(computer.fingerprint, true),
              }),
            ],
          })
        : null,
    ]);
  }
  return shownBanners;
}

// Only the computer in use has displays to rearrange, so the notice needs one to point at. A notice
// MonHop is already settling is a line in that computer's inspector instead.
function displayNoticeBanner(ctx) {
  const { actions, busy, peerName, sharing, activeComputer } = ctx;
  const notice = sharing.view?.displayNotice;
  if (!notice || !activeComputer) return null;
  const copy = displayNoticeCopy(notice.kind, peerName);
  if (!copy || copy.presentation !== "banner") return null;
  return banner({
    id: "canvas-display-notice",
    tone: "notice",
    iconName: "monitor",
    title: copy.title,
    body: copy.body,
    actions: [
      button(copy.secondaryLabel, {
        variant: "ghost",
        size: "sm",
        disabled: busy,
        focusKey: "canvas-display-notice-dismiss",
        onClick: actions.dismissDisplayNotice,
      }),
      button(copy.primaryLabel, {
        size: "sm",
        disabled: busy,
        focusKey: "canvas-display-notice-arrange",
        onClick: actions.changeLayout,
      }),
    ],
  });
}

// Shown whenever a downloaded build is ready, whether or not a computer is paired or in use.
function updateReadyBanner({ updates, actions }) {
  const { view, pending } = updates;
  if (view.phase !== "ready") return null;
  const hint = installHint(view);
  return banner({
    id: "canvas-update-ready",
    tone: "success",
    iconName: "refresh-cw",
    title: `MonHop ${view.availableVersion} is ready`,
    body: hint || UPDATE_READY_NOTE,
    actions: [
      button("Restart to update", {
        id: "canvas-updates-install",
        size: "sm",
        disabled: pending || !canInstall(view),
        busy: pending,
        onClick: actions.installUpdate,
      }),
    ],
  });
}

// --- the read-only views ----------------------------------------------------------

function localPlatformOf(ctx) {
  return ctx.state.snapshot?.platform ?? ctx.platform;
}

function localName(ctx) {
  return platformLabel(localPlatformOf(ctx), true);
}

// The sidebar's own words for this computer, so its label never disagrees with its row.
function localStatus(ctx) {
  const local = ctx.studio.rows.find((entry) => entry.kind === "local");
  return local ? { label: local.label, tone: local.tone } : null;
}

function peerStatus(ctx, computer) {
  const status = computerStatus(computer, ctx.sharing.view, ctx.active, localName(ctx));
  return { label: status.label, tone: status.tone, offline: OFFLINE_KEYS.has(status.key) };
}

// A pairing on screen draws the place the new computer will take, and quiets the others.
function pairingLive(ctx) {
  return ctx.studio.selection.kind === "new" || ctx.pairing?.view?.phase === "showing";
}

function savedView(ctx, stage) {
  const computer = ctx.studio.canvasComputer;
  const element = el("div", { className: "canvas-view", dataset: { view: "saved" } });
  const toast = el("p", {
    className: "canvas-toast",
    attrs: { role: "status" },
    children: [badge("Applied on both computers", "success", "check"), el("span")],
  });
  let setupKey = null;
  let picture = null;
  const pictureNames = (context) => ({
    local: localName(context),
    peer: displayName(computer),
    localPlatform: localPlatformOf(context),
    peerPlatform: computer.platform,
    statuses: { local: localStatus(context), peer: peerStatus(context, computer) },
    quiet: pairingLive(context) ? ["peer"] : [],
    slot: pairingLive(context) ? SLOT_COPY : null,
  });
  function update(context) {
    const current = findComputer(context.computers, computer.fingerprint) ?? computer;
    const key = JSON.stringify(current.setup ?? null);
    const names = pictureNames(context);
    if (key !== setupKey || !picture?.update) {
      setupKey = key;
      const next = createDashboardArrangement(current.setup, {
        ...names,
        motionKey: `canvas-${computer.fingerprint}-layout`,
        stage: stageSize(element.parentElement ?? element),
        motion: MOTION,
      });
      if (picture) picture.element.replaceWith(next.element);
      else element.prepend(next.element);
      picture = next;
    } else picture.update(names);
    const applied = hasAppliedLayout(context.sharing) && !isSessionStatus(context.status);
    toast.lastElementChild.textContent = applied
      ? `Sharing with ${context.peerName} starts in a moment.`
      : "";
    toast.dataset.shown = String(applied);
  }
  setupKey = JSON.stringify(computer.setup ?? null);
  picture = createDashboardArrangement(computer.setup, {
    ...pictureNames(ctx),
    motionKey: `canvas-${computer.fingerprint}-layout`,
    stage,
    motion: MOTION,
  });
  element.append(picture.element, toast);
  update(ctx);
  return { element, update, refit: (size) => picture?.refit(size) };
}

// A canvas without an arrangement draws this computer's own displays above a card that says what
// comes next. `card(context)` builds that card; `slot(context)` names the place still empty.
function pictureView(ctx, stage, { view, card, slot }) {
  const host = el("div", { className: "canvas-picture" });
  const element = el("div", { className: "canvas-view", dataset: { view }, children: [host] });
  let cardHeight = CARD_ESTIMATE;
  const pictureNames = (context) => ({
    local: localName(context),
    localPlatform: localPlatformOf(context),
    statuses: { local: localStatus(context) },
    slot: slot(context),
    card: cardHeight,
  });
  let displaysKey = null;
  let picture = null;
  let cardKey = null;
  let cardNode = null;
  // The card sits under the picture, where the picture's fit puts it, and glides with it.
  function placeCard() {
    const top = picture?.cardTop();
    if (top !== null && top !== undefined) element.style.setProperty("--canvas-card-y", `${top}px`);
  }
  function measureCard({ still = false } = {}) {
    const height = cardNode?.isConnected ? cardNode.offsetHeight : 0;
    if (height > 0 && Math.abs(height - cardHeight) >= 1) {
      cardHeight = height;
      picture?.setCard(height, { still });
    }
    placeCard();
  }
  function update(context, size = null) {
    const reported = context.state.snapshot?.displays ?? [];
    const key = JSON.stringify(reported);
    const names = pictureNames(context);
    if (key !== displaysKey) {
      displaysKey = key;
      picture = createLocalPicture(reported, {
        ...names,
        motionKey: `canvas-${view}-local`,
        stage: size ?? stageSize(element.parentElement ?? element),
        motion: MOTION,
      });
      host.replaceChildren(...(picture ? [picture.element] : []));
      element.dataset.picture = String(Boolean(picture));
    } else picture?.update(names);
    const built = card(context);
    if (built.key !== cardKey) {
      cardKey = built.key;
      const next = el("div", { className: "canvas-card", children: [built.node] });
      if (cardNode) cardNode.replaceWith(next);
      else element.append(next);
      cardNode = next;
      measureCard();
    } else built.refresh?.(cardNode.firstElementChild);
    placeCard();
  }
  update(ctx, stage);
  return {
    element,
    update,
    mounted: () => measureCard({ still: true }),
    refit: (size) => {
      picture?.refit(size);
      placeCard();
    },
  };
}

function placeholder({ title, body, actions = [] }) {
  return el("div", {
    className: "canvas-placeholder",
    children: [
      el("strong", { text: title }),
      body ? el("p", { text: body }) : null,
      actions.length ? el("div", { className: "card-actions", children: actions }) : null,
    ],
  });
}

// With nothing paired the canvas shows this computer's own displays, as the last check reported them.
function emptyView(ctx, stage) {
  return pictureView(ctx, stage, {
    view: "empty",
    slot: (context) => (pairingLive(context) ? SLOT_COPY : null),
    card: (context) => {
      const local = localName(context);
      const known = Boolean(context.state.snapshot?.displays?.length);
      const pairing = pairingLive(context);
      const title = known ? `${local}'s displays` : "No computer paired yet";
      if (pairing)
        return {
          key: `pairing:${title}`,
          node: placeholder({
            title,
            body: "The new computer's displays appear beside these once it pairs.",
          }),
        };
      const add = button("Add a computer", {
        disabled: context.busy,
        focusKey: "canvas-add-computer",
        onClick: () => context.actions.select("new"),
      });
      add.prepend(el("span", { className: "add-glyph", attrs: { "aria-hidden": "true" } }));
      return {
        key: `add:${title}`,
        node: placeholder({
          title,
          body: "Add a computer, then place its displays next to these so the pointer crosses where you expect.",
          actions: [add],
        }),
        refresh: (node) => {
          const control = node.querySelector('[data-focus-key="canvas-add-computer"]');
          if (control) control.disabled = context.busy;
        },
      };
    },
  });
}

function unarrangedView(ctx, stage) {
  const computer = ctx.studio.canvasComputer;
  return pictureView(ctx, stage, {
    view: "unarranged",
    slot: () => ({ title: displayName(computer), detail: "Not placed yet" }),
    card: (context) => {
      const arrange = button("Arrange displays", {
        iconName: "monitor",
        disabled: context.busy || !context.studio.canArrange,
        focusKey: "canvas-arrange",
        onClick: () => context.actions.arrangeWith(computer.fingerprint),
      });
      return {
        key: "unarranged",
        node: placeholder({
          title: "Not arranged yet",
          body: `Place ${displayName(computer)}'s displays next to this computer's, so the pointer crosses where you expect.`,
          actions: [arrange],
        }),
        refresh: (node) => {
          const control = node.querySelector('[data-focus-key="canvas-arrange"]');
          if (control) control.disabled = context.busy || !context.studio.canArrange;
        },
      };
    },
  });
}

// Arranging needs the link with the other computer; until it is up the canvas says what it waits for.
function waitingView(ctx, stage) {
  if (isConnected(ctx.sharing)) {
    const element = el("div", {
      className: "canvas-view",
      dataset: { view: "blocked" },
      children: [
        el("div", {
          className: "canvas-card",
          children: [
            placeholder({
              title: "The displays cannot be arranged yet",
              body:
                arrangementForSharing(ctx.sharing)?.message ||
                "Reconnect every computer, then try again.",
            }),
          ],
        }),
      ],
    });
    return { element, update() {} };
  }
  return pictureView(ctx, stage, {
    view: "waiting",
    slot: (context) => ({ title: context.peerName, detail: "Its displays appear once connected" }),
    card: (context) => {
      const { status } = context;
      const tone = status.tone === "error" ? "error" : "checking";
      const title = status.key === "connected" ? "Opening the arrangement" : status.label;
      return {
        key: `${tone}|${title}|${status.detail}`,
        node: stateCard({
          id: "canvas-waiting",
          tone,
          iconName: "triangle-alert",
          title,
          detail: status.detail,
        }),
      };
    },
  });
}

// --- arranging ----------------------------------------------------------------

function editView(ctx) {
  const element = el("div", { className: "canvas-view", dataset: { view: "edit" } });
  let mountedEditor = null;
  function update(context) {
    const editor = liveEditor(context);
    if (mountedEditor !== editor.element) {
      element.replaceChildren(editor.element);
      mountedEditor = editor.element;
    }
    // The editor is fed its new state only once it is in the document, so a moved group animates there.
    editor.mounted();
  }
  let first = liveEditor(ctx);
  element.append(first.element);
  mountedEditor = first.element;
  return {
    element,
    update,
    mounted: () => {
      first?.mounted();
      first = null;
    },
  };
}

// Reset returns to the applied arrangement, so it stays off while the draft already matches it.
function resetControl({ sharing, actions, busy }) {
  const target = arrangementResetTarget(sharing);
  const enabled = !busy && canResetArrangement(sharing);
  return {
    enabled,
    hint:
      target?.origin === "applied"
        ? "Go back to the arrangement applied on both computers"
        : "Go back to the default side-by-side placement",
    apply: () => {
      if (target) actions.resetArrangement();
    },
  };
}

// Apply and the way out, the editor's own Reset and Fit, and the saved arrangements and the
// displays in use a press away in the drawer above.
function applyBar(ctx, mode) {
  const { sharing, actions, busy, arrange, canvasDrawer } = ctx;
  const applying = sharing.pending?.kind === "apply" || isSyncing(sharing);
  const editing = mode === "edit";
  const toggles = editing
    ? [
        drawerToggle(ctx, "saved", savedLabel(sharing)),
        Object.values(displayUseChoices(sharing)).some((list) => list.length)
          ? drawerToggle(ctx, "use", "Displays in use")
          : null,
      ]
    : [];
  return el("div", {
    className: "apply-bar",
    attrs: { role: "region", "aria-label": "Arranging" },
    children: [
      el("div", {
        className: "apply-bar-status",
        children: [
          editing
            ? syncStatus(ctx)
            : el("p", {
                className: "progress-line",
                text: "Apply turns on once both computers connect.",
              }),
          sharing.message && editing ? note(sharing.message, "danger") : null,
        ],
      }),
      // The tools wrap as one group and the way out as another, so Apply never strays from Done.
      el("div", {
        className: "apply-bar-tools",
        children: [
          el("div", { className: "apply-bar-drawers", children: toggles }),
          editing ? live?.controls : null,
        ],
      }),
      el("div", {
        className: "apply-bar-actions",
        children: [
          arrange.leavable
            ? button(sharing.view?.editing ? "Done" : "Cancel", {
                variant: "ghost",
                size: "sm",
                disabled: busy && !applying,
                focusKey: "apply-bar-leave",
                onClick: actions.leaveArrange,
              })
            : null,
          button(applying ? "Applying…" : "Apply on both computers", {
            busy: applying,
            size: "sm",
            disabled: !editing || applying || busy || !canApplySetup(sharing),
            focusKey: "displays-apply",
            onClick: actions.applySetup,
          }),
        ],
      }),
    ],
  });
  function drawerToggle(context, kind, label) {
    const open = canvasDrawer === kind;
    const toggle = button(label, {
      variant: "ghost",
      size: "sm",
      pressed: open,
      focusKey: `apply-bar-${kind}`,
      onClick: () => context.actions.toggleCanvasDrawer(kind),
    });
    toggle.classList.add("apply-bar-drawer-toggle");
    toggle.setAttribute("aria-expanded", String(open));
    const chevron = icon("chevron-right", 12);
    chevron.classList.add("apply-bar-chevron");
    // The toggle is rebuilt on every poll, so the chevron's turn carries on from when it was pressed.
    const elapsed = sinceChanged(`apply-bar-${kind}-open`, open);
    if (elapsed < motionMs("--motion-spring")) {
      chevron.dataset.enter = "true";
      chevron.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
    }
    toggle.append(chevron);
    return toggle;
  }
}

function savedLabel(sharing) {
  const count = sharing.arrangements.length;
  return count ? `Saved arrangements (${count})` : "Saved arrangements";
}

function drawerContent(ctx) {
  if (ctx.canvasDrawer === "saved")
    return el("div", {
      className: "canvas-drawer",
      dataset: { kind: "saved" },
      children: [
        el("div", {
          className: "canvas-drawer-heading",
          children: [
            el("strong", { text: "Saved arrangements" }),
            el("span", { text: "Load one, or save the arrangement above under a name." }),
          ],
        }),
        ...arrangementsContent(ctx),
      ],
    });
  if (ctx.canvasDrawer === "use" && live?.usePanel)
    return el("div", {
      className: "canvas-drawer",
      dataset: { kind: "use" },
      children: [live.usePanel],
    });
  return null;
}

// The overall state on the left, and the way to take input back, drawn as the keys to hold.
function renderStatusBar(nodes, ctx) {
  const { status } = ctx;
  nodes.statusBarDot.dataset.tone = status.tone;
  const text = nodes.statusBarText;
  if (!text.querySelector(".status-bar-label")) {
    text.replaceChildren(
      el("span", { className: "status-bar-label" }),
      el("span", { className: "status-bar-detail" }),
    );
  }
  const detail = status.detail && status.detail !== status.label ? status.detail : "";
  setLabel(text.querySelector(".status-bar-label"), status.label);
  setLabel(text.querySelector(".status-bar-detail"), detail);
  text.title = detail ? `${status.label} · ${detail}` : status.label;
  renderEscape(nodes.statusBarText.closest(".status-bar"), ctx.platform);
}

function renderEscape(bar, platform) {
  const escape = bar?.querySelector(".status-bar-escape");
  if (!escape || escapePlatform === platform) return;
  escapePlatform = platform;
  const windows = platform === "windows";
  const control = windows ? "Ctrl" : "⌃";
  escape.title = ESCAPE_SENTENCE;
  escape.replaceChildren(
    el("span", { className: "visually-hidden", text: ESCAPE_SENTENCE }),
    el("span", {
      className: "status-bar-keys",
      attrs: { "aria-hidden": "true" },
      children: [
        el("span", { text: "Hold" }),
        el("span", {
          className: "keycaps",
          children: [keycap(control), keycap(control), keycap(windows ? "Esc" : "esc")],
        }),
        el("span", { className: "status-bar-escape-long", text: "for 2 s to take input back" }),
        el("span", { className: "status-bar-escape-short", text: "for 2 s" }),
      ],
    }),
  );
}

function keycap(label) {
  return el("kbd", { className: "keycap", text: label });
}

// The app rebuilds this screen on every status poll; keeping the editor alive keeps a drag alive with it.
let live = null;

// Today's exactly-two-computer options (`localPlatform`/`peerPlatform`/`localLabel`/`peerLabel`) for
// up to two members, unchanged; past that the picture needs a member per computer, so this instead
// hands the view a `members` list (arrangement-view-model.mjs assigns each one the same "local",
// "peer", "peer-2".."peer-6" tones it always has) built from the connected group's own order.
function liveOptions(ctx) {
  const { sharing, computers } = ctx;
  const view = sharing.view;
  const group = groupFromView(view);
  if (!group || group.order.length <= 2)
    return {
      localPlatform: view.localPlatform,
      peerPlatform: view.peerPlatform,
      localLabel: platformLabel(view.localPlatform, true),
      peerLabel: ctx.peerName,
    };
  return { members: memberOptions(group, computers, ctx) };
}

// The label, platform and key for each member of the connected group, in the group's own order
// (local first): a paired computer's own name when one is found, `ctx.peerName`'s fallback chain
// otherwise, so a member MonHop has not read a name for yet still gets something to show.
function memberOptions(group, computers, ctx) {
  return group.order.map((memberKey) => {
    const member = group.byKey[memberKey];
    return {
      key: memberKey,
      platform: member.platform,
      label: member.local
        ? platformLabel(member.platform, true)
        : memberLabel(member, computers, ctx),
    };
  });
}

function memberLabel(member, computers, ctx) {
  const computer = member.fingerprint ? findComputer(computers, member.fingerprint) : null;
  return computer ? displayName(computer) : ctx.peerName;
}

// Every member is on the link while arranging, so each label's dot shows the same live state.
function labelTones(ctx, options) {
  const tone = ctx.status.tone;
  const keys = options.members?.map((member) => member.key) ?? ["local", "peer"];
  return Object.fromEntries(keys.map((key) => [key, { tone }]));
}

function liveEditor(ctx) {
  const { sharing, actions, busy } = ctx;
  const reset = resetControl(ctx);
  const arrangement = arrangementForSharing(sharing);
  const setup = liveOptions(ctx);
  const key = JSON.stringify(setup);
  const handlers = {
    onCommit: (placement, moving) => actions.commitArrangement(placement, moving),
    onReset: reset.apply,
    onShowMonitor: (monitor, side) => actions.showMonitorOn(monitor, side),
    onUseDisplay: (id, inUse) => actions.useDisplay(id, inUse),
  };
  const shared = sharedMonitorChoices(sharing);
  const inUse = displayUseChoices(sharing);
  const labelDetails = labelTones(ctx, setup);
  if (live?.key !== key) {
    releaseEditor();
    const view = createArrangementView({
      ...setup,
      ...handlers,
      arrangement,
      shared,
      inUse,
      labelDetails,
      disabled: busy,
      canReset: reset.enabled,
      resetHint: reset.hint,
    });
    // Reset and Fit ride in the Apply bar and the display switches in its drawer; the editor still
    // drives both wherever they sit.
    const controls = view.element.querySelector(".arrangement-controls");
    const usePanel = view.element.querySelector(".arrangement-use")?.parentElement ?? null;
    controls?.remove();
    usePanel?.remove();
    live = { key, editor: view, handle: null, controls, usePanel };
  }
  const editor = live.editor;
  // app.js destroys whatever view it holds on the next render; only the handle it still holds may tear the editor down.
  const handle = {
    element: editor.element,
    destroy() {
      if (live?.handle === handle) releaseEditor();
    },
  };
  live.handle = handle;
  actions.setArrangementView(handle);
  return {
    element: editor.element,
    mounted: () =>
      editor.update({
        arrangement,
        shared,
        inUse,
        labelDetails,
        disabled: busy,
        canReset: reset.enabled,
        resetHint: reset.hint,
        handlers,
        members: setup.members,
      }),
  };
}

function releaseEditor() {
  live?.editor.destroy();
  live = null;
}

function syncStatus(ctx) {
  const { sharing, actions, busy } = ctx;
  const sync = sharing.view?.sync ?? { state: "idle", message: "" };
  const line = el("div", { className: "progress-line" });
  if (sharing.pending?.kind === "apply" || sync.state === "sending") {
    line.dataset.tone = "busy";
    line.append(
      el("span", { className: "spinner" }),
      el("span", { text: "Applying on both computers…" }),
    );
  } else if (sync.state === "receiving") {
    line.dataset.tone = "busy";
    line.append(
      el("span", { className: "spinner" }),
      el("span", { text: `Receiving a layout from ${ctx.peerName}…` }),
    );
  } else if (hasAppliedCurrentLayout(sharing)) {
    line.dataset.tone = "success";
    line.append(badge("Applied on both computers", "success", "check"));
  } else if (sync.state === "applied")
    line.append(
      el("span", { text: "You changed the layout. Apply again to update both computers." }),
    );
  else if (sync.state === "rejected") {
    line.dataset.tone = "danger";
    const reason = sync.message || "The other computer could not use this layout.";
    line.append(
      badge("Not applied", "danger"),
      el("span", { className: "progress-line-text", text: reason, attrs: { title: reason } }),
      button("Try again", {
        variant: "ghost",
        size: "sm",
        iconName: "refresh-cw",
        disabled: !canApplySetup(sharing) || busy,
        focusKey: "apply-bar-retry",
        onClick: actions.applySetup,
      }),
    );
  } else line.append(el("span", { text: "Apply saves this layout on both computers." }));
  return line;
}

// --- saved arrangements ---------------------------------------------------

// The screen is rebuilt on every poll, so the typed name and a pending delete live outside the DOM.
let nameDraft = "";
let pendingDelete = null;

// The library lists its oldest entry first; here and in every computer's inspector the newest leads.
function arrangementsContent(ctx) {
  const { sharing } = ctx;
  const entries = newestFirst(sharing.arrangements);
  if (pendingDelete && !entries.some((entry) => entry.name === pendingDelete)) pendingDelete = null;
  return [
    entries.length
      ? rows(entries.map((entry) => arrangementRow(ctx, entry)))
      : note(
          "MonHop remembers each arrangement you apply, for its display configuration. You can also save one under a name to switch between several later.",
        ),
    saveForm(ctx),
  ];
}

function arrangementRow(ctx, entry) {
  const { sharing, actions, busy } = ctx;
  // Whether it fits is the "Fits now" chip's job alone, so the line never says it twice.
  const detail = `${entry.crossings} crossing${entry.crossings === 1 ? "" : "s"}`;
  const key = rowKey(entry.name);
  const deleting = pendingDelete === entry.name;
  // Arming keeps the button's width, so Load and Overwrite beside it stay put.
  const label = (armed) =>
    stableLabel(`${key}-delete`, ["Delete", "Confirm delete"], armed ? 1 : 0);
  const remove = button("", {
    variant: deleting ? "destructive" : "ghost",
    size: "sm",
    disabled: busy,
    focusKey: `${key}-delete`,
    pressed: deleting,
    onClick: () => {
      if (pendingDelete !== entry.name) {
        pendingDelete = entry.name;
        remove.querySelector(".stable-label").replaceWith(label(true));
        remove.dataset.variant = "destructive";
        remove.setAttribute("aria-pressed", "true");
        return;
      }
      pendingDelete = null;
      void actions.deleteArrangement(entry.name);
    },
  });
  remove.append(label(deleting));
  return row({
    title: entry.name,
    detail,
    leading: layoutChipStrip(key, entry),
    actions: [
      button("Load", {
        variant: "outline",
        size: "sm",
        disabled: busy || !canLoadArrangement(sharing, entry.name),
        focusKey: `${key}-load`,
        onClick: () => void actions.loadSavedArrangement(entry.name),
      }),
      button("Overwrite", {
        variant: "ghost",
        size: "sm",
        disabled: busy || !canSaveArrangement(sharing, entry.name),
        focusKey: `${key}-overwrite`,
        onClick: () => void actions.saveArrangement(entry.name),
      }),
      remove,
    ],
  });
}

function saveForm(ctx) {
  const { sharing, actions, busy } = ctx;
  const savable = !busy && canSaveArrangement(sharing, "draft");
  const input = el("input", {
    className: "input",
    attrs: {
      type: "text",
      id: "arrangement-name",
      placeholder: "Name this arrangement",
      maxlength: MAX_ARRANGEMENT_NAME,
      autocomplete: "off",
      "aria-label": "Name for the current arrangement",
    },
  });
  input.value = nameDraft;
  input.disabled = !savable;
  const save = button("Save current", {
    size: "sm",
    disabled: !savable || !normalizeArrangementName(nameDraft),
    focusKey: "arrangement-save",
    onClick: () => submit(),
  });
  input.addEventListener("input", () => {
    nameDraft = input.value;
    save.disabled = !savable || !normalizeArrangementName(nameDraft);
  });
  input.addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    event.preventDefault();
    submit();
  });
  const submit = () => {
    const name = normalizeArrangementName(nameDraft);
    if (!name || save.disabled) {
      input.focus({ preventScroll: true });
      return;
    }
    nameDraft = "";
    void actions.saveArrangement(name);
  };
  return el("div", {
    className: "arrangement-save",
    children: [
      el("div", { className: "arrangement-save-row", children: [input, save] }),
      note(
        savable
          ? "Saves the arrangement shown above under this name. An existing name is replaced."
          : "Arrange the displays so they touch, then save the arrangement under a name.",
      ),
    ],
  });
}

function rowKey(name) {
  return `arrangement-${Array.from(name)
    .map((char) => char.codePointAt(0).toString(36))
    .join("-")
    .slice(0, 80)}`;
}
