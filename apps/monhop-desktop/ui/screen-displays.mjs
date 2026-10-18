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
import { displaysFreshness, newestFirst, resolveEnabledList } from "./computer-card-model.mjs";
import { layoutChipStrip } from "./computer-card.mjs";
import { presence } from "./accordion.mjs";
import { createDashboardArrangement } from "./dashboard-arrangement.mjs";
import { canInstall, installHint } from "./updates-model.mjs";
import {
  badge,
  button,
  clear,
  el,
  icon,
  note,
  row,
  rows,
  setLabel,
  stableLabel,
  stateCard,
  swap,
} from "./dom.mjs";

// The canvas: its banners, the arrangement (read only, or the live editor while arranging), the
// Apply bar and the status bar.
export function renderCanvas(nodes, ctx) {
  renderBanners(nodes.canvasBanners, ctx);
  const mode = canvasMode(ctx);
  nodes.canvasPane.dataset.mode = mode;
  let mounted = null;
  let content;
  if (mode === "edit") {
    const built = editorContent(ctx);
    content = built.element;
    mounted = built.mounted;
  } else {
    releaseEditor();
    content =
      { saved: savedContent, waiting: waitingContent, unarranged: unarrangedContent }[mode]?.(
        ctx,
      ) ?? emptyContent(ctx);
  }
  clear(nodes.canvasStage);
  nodes.canvasStage.append(swap("canvas-stage", content, mode, { block: true }));
  clear(nodes.canvasDock);
  const drawer = presence("canvas-drawer", mode === "edit" ? drawerContent(ctx) : null);
  if (drawer) nodes.canvasDock.append(drawer);
  const bar = presence("apply-bar", ctx.arrange.active ? applyBar(ctx, mode) : null);
  if (bar) nodes.canvasDock.append(bar);
  renderStatusBar(nodes, ctx);
  // The editor is fed its new state only once it is back in the document, so a moved group animates there.
  mounted?.();
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
          body ? el("p", { text: body }) : null,
        ],
      }),
      actions.length ? el("div", { className: "banner-actions", children: actions }) : null,
    ],
  });
}

// A computer switched on that cannot connect says so above the canvas, with a way to retry or let go.
function attentionBanners(ctx) {
  const { computers, sharing, active, actions, busy } = ctx;
  const local = platformLabel(ctx.state.snapshot?.platform ?? ctx.platform, true);
  const enabled = resolveEnabledList(sharing.view, computers, active);
  const known = new Set(enabled);
  const shown = [];
  for (const computer of computers.items) {
    const key = `canvas-attention-${computer.fingerprint}`;
    const status = known.has(computer.fingerprint)
      ? computerStatus(computer, sharing.view, active, local)
      : null;
    const attention = status && ["error", "attention"].includes(status.key);
    shown.push([
      key,
      attention
        ? banner({
            tone: status.key === "error" ? "danger" : "warning",
            iconName: "triangle-alert",
            title: `${displayName(computer)}: ${status.label}`,
            body: status.detail,
            actions: [
              button("Try again", {
                variant: "secondary",
                size: "sm",
                iconName: "refresh-cw",
                disabled: busy,
                focusKey: `${key}-retry`,
                onClick: () => actions.setComputerEnabled(computer.fingerprint, true),
              }),
              button("Pause", {
                variant: "ghost",
                size: "sm",
                disabled: busy,
                focusKey: `${key}-pause`,
                onClick: () => actions.setComputerEnabled(computer.fingerprint, false),
              }),
            ],
          })
        : null,
    ]);
  }
  return shown;
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
      button(copy.primaryLabel, {
        size: "sm",
        disabled: busy,
        focusKey: "canvas-display-notice-arrange",
        onClick: actions.changeLayout,
      }),
      button(copy.secondaryLabel, {
        variant: "ghost",
        size: "sm",
        disabled: busy,
        focusKey: "canvas-display-notice-dismiss",
        onClick: actions.dismissDisplayNotice,
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
    body: hint || "It installs when you quit MonHop, or now.",
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

// --- the arrangement ------------------------------------------------------------

function savedContent(ctx) {
  const { studio, sharing, status, peerName } = ctx;
  const computer = studio.canvasComputer;
  const localPlatform = ctx.state.snapshot?.platform ?? ctx.platform;
  const applied = hasAppliedLayout(sharing) && !isSessionStatus(status);
  return el("div", {
    className: "canvas-view",
    dataset: { view: "saved" },
    children: [
      createDashboardArrangement(computer.setup, {
        local: platformLabel(localPlatform, true),
        peer: displayName(computer),
        localPlatform,
        peerPlatform: computer.platform,
        motionKey: `canvas-${computer.fingerprint}-layout`,
        compactLegend: true,
        caption: displaysFreshness(computer.setup),
      }),
      applied
        ? note(`Applied on both computers. Sharing with ${peerName} starts in a moment.`, "success")
        : null,
    ],
  });
}

function unarrangedContent(ctx) {
  const { studio, actions, busy } = ctx;
  const computer = studio.canvasComputer;
  return placeholder({
    title: "Not arranged yet",
    body: `Place ${displayName(computer)}'s displays next to this computer's, so the pointer crosses where you expect.`,
    actions: [
      button("Arrange displays", {
        iconName: "monitor",
        disabled: busy || !studio.canArrange,
        focusKey: "canvas-arrange",
        onClick: () => actions.arrangeWith(computer.fingerprint),
      }),
    ],
  });
}

// Arranging needs the link with the other computer; until it is up the canvas says what it waits for.
function waitingContent(ctx) {
  const { sharing, status } = ctx;
  if (isConnected(sharing))
    return placeholder({
      title: "The displays cannot be arranged yet",
      body: arrangementForSharing(sharing)?.message || "Reconnect every computer, then try again.",
    });
  return el("div", {
    className: "canvas-view",
    dataset: { view: "waiting" },
    children: [
      stateCard({
        id: "canvas-waiting",
        tone: status.tone === "error" ? "error" : "checking",
        iconName: "triangle-alert",
        title: status.key === "connected" ? "Opening the arrangement" : status.label,
        detail: status.detail,
      }),
    ],
  });
}

// With nothing paired the canvas shows this computer's own displays, as the last check reported them.
function emptyContent(ctx) {
  const displays = ctx.state.snapshot?.displays ?? [];
  const local = platformLabel(ctx.state.snapshot?.platform ?? ctx.platform, true);
  const widest = Math.max(1, ...displays.map((display) => display.width));
  return el("div", {
    className: "canvas-view",
    dataset: { view: "empty" },
    children: [
      displays.length
        ? el("div", {
            className: "local-displays",
            attrs: { role: "img", "aria-label": `${local}'s displays` },
            children: displays.map((display) => localDisplay(display, widest)),
          })
        : null,
      placeholder({
        title: displays.length ? `${local}'s displays` : "No computer paired yet",
        body: "Add a computer, then place its displays next to these so the pointer crosses where you expect.",
        actions: [
          button("Add a computer", {
            disabled: ctx.busy,
            focusKey: "canvas-add-computer",
            onClick: () => ctx.actions.select("new"),
          }),
        ],
      }),
    ],
  });
}

function localDisplay(display, widest) {
  const tile = el("div", {
    className: "local-display",
    dataset: { primary: String(display.primary) },
    children: [
      el("strong", { text: display.name || "Display" }),
      el("span", { text: `${display.width} × ${display.height}` }),
    ],
  });
  tile.style.setProperty("--display-share", String(display.width / widest));
  tile.style.setProperty("--display-ratio", `${display.width} / ${Math.max(1, display.height)}`);
  return tile;
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

// --- arranging ----------------------------------------------------------------

function editorContent(ctx) {
  const reset = resetControl(ctx);
  const arrangement = arrangementForSharing(ctx.sharing);
  const editor = liveEditor(ctx, reset, arrangement);
  return {
    element: el("div", {
      className: "canvas-view",
      dataset: { view: "edit" },
      children: [editor.element],
    }),
    mounted: editor.mounted,
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

// Apply, leave, and the editor's own Reset and Fit, with the saved arrangements and the displays in
// use a press away in the drawer above.
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
          editing ? syncStatus(ctx) : note(ctx.status.detail),
          sharing.message && editing ? note(sharing.message, "danger") : null,
        ],
      }),
      // The tools wrap as one group and the way out as another, so Apply never strays from Done.
      el("div", {
        className: "apply-bar-tools",
        children: [...toggles, editing ? live?.controls : null],
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
    return button(label, {
      variant: "ghost",
      size: "sm",
      pressed: canvasDrawer === kind,
      focusKey: `apply-bar-${kind}`,
      onClick: () => context.actions.toggleCanvasDrawer(kind),
    });
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
      children: arrangementsContent(ctx),
    });
  if (ctx.canvasDrawer === "use" && live?.usePanel)
    return el("div", {
      className: "canvas-drawer",
      dataset: { kind: "use" },
      children: [live.usePanel],
    });
  return null;
}

function renderStatusBar(nodes, ctx) {
  const { status } = ctx;
  nodes.statusBarDot.dataset.tone = status.tone;
  setLabel(
    nodes.statusBarText,
    status.detail && status.detail !== status.label
      ? `${status.label} · ${status.detail}`
      : status.label,
  );
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
      localLabel: `${platformLabel(view.localPlatform)} · This computer`,
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
        ? `${platformLabel(member.platform)} · This computer`
        : memberLabel(member, computers, ctx),
    };
  });
}

function memberLabel(member, computers, ctx) {
  const computer = member.fingerprint ? findComputer(computers, member.fingerprint) : null;
  return computer ? displayName(computer) : ctx.peerName;
}

function liveEditor(ctx, reset, arrangement) {
  const { sharing, actions, busy } = ctx;
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
  if (live?.key !== key) {
    releaseEditor();
    const view = createArrangementView({
      ...setup,
      ...handlers,
      arrangement,
      shared,
      inUse,
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
  if (sharing.pending?.kind === "apply" || sync.state === "sending")
    line.append(
      el("span", { className: "spinner" }),
      el("span", { text: "Applying on both computers…" }),
    );
  else if (sync.state === "receiving")
    line.append(
      el("span", { className: "spinner" }),
      el("span", { text: `Receiving a layout from ${ctx.peerName}…` }),
    );
  else if (hasAppliedCurrentLayout(sharing))
    line.append(badge("Applied on both computers", "success", "check"));
  else if (sync.state === "applied")
    line.append(
      el("span", { text: "You changed the layout. Apply again to update both computers." }),
    );
  else if (sync.state === "rejected")
    line.append(
      badge("Not applied", "danger"),
      el("span", { text: sync.message || "The other computer could not use this layout." }),
      button("Try again", {
        variant: "ghost",
        size: "sm",
        disabled: !canApplySetup(sharing) || busy,
        onClick: actions.applySetup,
      }),
    );
  else
    line.append(
      el("span", { text: "Apply sends this layout to both computers. They save the same layout." }),
    );
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
