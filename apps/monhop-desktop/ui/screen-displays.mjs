import { isSessionStatus } from "./computer-status.mjs";
import { platformLabel } from "./pairing-model.mjs";
import { MODE_INFO } from "./arrangement-model.mjs";
import {
  MAX_ARRANGEMENT_NAME,
  arrangementForSharing,
  arrangementResetTarget,
  canApplySetup,
  canChooseSource,
  canLoadArrangement,
  canResetArrangement,
  canSaveArrangement,
  displayUseChoices,
  hasAppliedCurrentLayout,
  hasAppliedLayout,
  isConnected,
  isSyncing,
  normalizeArrangementName,
  sharedMonitorChoices,
} from "./sharing-model.mjs";
import { createArrangementView } from "./arrangement-view.mjs";
import { createAccordion } from "./accordion.mjs";
import { createDashboardArrangement } from "./dashboard-arrangement.mjs";
import { autostartDescription } from "./autostart-model.mjs";
import {
  badge,
  button,
  card,
  clear,
  el,
  iconButton,
  note,
  platformGlyph,
  row,
  rows,
  stateCard,
  statusChip,
  switchRow,
} from "./dom.mjs";

const LOCKED_DESCRIPTION =
  "Place each computer's displays so the pointer crosses where you expect. Both computers save the same layout.";

export function renderDisplays(nodes, ctx) {
  const { sharing, actions, busy, gates } = ctx;
  clear(nodes.displaysContent);
  clear(nodes.displaysActions);
  if (gates.displays.locked) {
    releaseEditor();
    nodes.displaysContent.append(
      card({ title: "Display layout", description: LOCKED_DESCRIPTION }),
    );
    return;
  }
  if (!isConnected(sharing)) {
    releaseEditor();
    nodes.displaysContent.append(offLinkCard(ctx));
    return;
  }
  if (sharing.view.editing)
    nodes.displaysActions.append(
      button("Done", {
        variant: "outline",
        size: "sm",
        disabled: busy,
        focusKey: "displays-done",
        onClick: actions.endLayoutEdit,
      }),
    );
  const reset = resetControl(ctx);
  nodes.displaysActions.append(
    iconButton({
      id: "arrange-reset",
      label: reset.hint,
      iconName: "rotate-ccw",
      size: "sm",
      disabled: !reset.enabled,
      onClick: reset.apply,
    }),
  );
  nodes.displaysContent.append(sourceCard(ctx));
  let mounted = null;
  if (sharing.source) {
    const built = arrangementCard(ctx, reset);
    nodes.displaysContent.append(built.element);
    mounted = built.mounted;
  } else {
    releaseEditor();
  }
  nodes.displaysContent.append(arrangementsDisclosure(ctx));
  // The editor is fed its new state only once it is back in the document, so a moved group animates there.
  mounted?.();
}

// Without the setup link there is nothing to drag: show what is saved and how to get the link back.
function offLinkCard(ctx) {
  const { sharing, actions, busy, activeComputer, status, peerName } = ctx;
  if (hasAppliedLayout(sharing) && !isSessionStatus(status))
    return stateCard({
      id: "displays-applied",
      tone: "active",
      iconName: "keyboard",
      title: "Applied on both computers",
      detail: `Both computers saved the same layout. Sharing with ${peerName} starts in a moment.`,
    });
  if (!isSessionStatus(status))
    return stateCard({
      id: "displays-waiting",
      tone: "checking",
      title: status.label,
      detail: status.detail,
    });
  const children = activeComputer?.setup?.saved
    ? [
        createDashboardArrangement(activeComputer.setup, {
          local: platformLabel(ctx.state.snapshot?.platform ?? ctx.platform, true),
          peer: peerName,
          localPlatform: ctx.state.snapshot?.platform ?? ctx.platform,
          peerPlatform: activeComputer.platform,
        }),
      ]
    : [note("No layout saved yet.")];
  return card({
    title: "Display layout",
    description: `Saved on this computer and on ${peerName}. Changing it pauses sharing until you apply.`,
    children,
    actions: [
      button("Change layout", {
        disabled: busy,
        focusKey: "displays-change",
        onClick: () => actions.beginLayoutEdit(),
      }),
    ],
  });
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

function sourceCard(ctx) {
  const { sharing, state, actions, busy, peerName } = ctx;
  const view = sharing.view;
  const field = el("fieldset", {
    className: "source-field",
    attrs: { id: "sharing-source-platform", "aria-label": "Input computer" },
  });
  field.disabled = busy || !canChooseSource(sharing);
  const localPlatform = view?.localPlatform ?? state.snapshot?.platform;
  const peerPlatform = view?.peerPlatform ?? (localPlatform === "macos" ? "windows" : "macos");
  for (const [side, platform, label, detail] of [
    [
      "local",
      localPlatform,
      platformLabel(localPlatform, true),
      `Sends its keyboard and mouse to ${peerName}`,
    ],
    [
      "peer",
      peerPlatform,
      peerName,
      `Sends its keyboard and mouse to this ${platformLabel(localPlatform)}`,
    ],
  ]) {
    const id = `sharing-source-${side}`;
    const input = el("input", {
      attrs: { type: "radio", name: "sharing-source", id, value: side },
    });
    input.checked = sharing.source === side;
    input.disabled = field.disabled;
    input.dataset.focusKey = id;
    input.addEventListener("change", () => {
      if (input.checked) void actions.chooseSource(side);
    });
    field.append(
      el("label", {
        className: "source-choice",
        attrs: { for: id },
        children: [
          input,
          el("span", {
            className: "source-choice-card",
            children: [
              platformGlyph(platform),
              el("span", {
                className: "source-choice-copy",
                children: [el("strong", { text: label }), el("small", { text: detail })],
              }),
              el("span", { className: "source-choice-check", attrs: { "aria-hidden": "true" } }),
            ],
          }),
        ],
      }),
    );
  }
  return card({
    title: "Input computer",
    description:
      "The computer whose keyboard and mouse are shared. It always keeps controlling itself.",
    children: [field],
  });
}

function arrangementCard(ctx, reset) {
  const { sharing, actions, busy, autostart, platform } = ctx;
  const arrangement = arrangementForSharing(sharing);
  const children = [];
  let mounted = null;
  if (arrangement?.groups?.source && arrangement.groups?.destination) {
    const editor = liveEditor(ctx, reset, arrangement);
    children.push(editor.element);
    mounted = editor.mounted;
  } else {
    releaseEditor();
    children.push(note(arrangement?.message || "The displays cannot be arranged yet.", "danger"));
  }
  children.push(syncStatus(ctx));
  // The page alert leaves this section to its own cards, so a failed command is reported right here.
  if (sharing.message) children.push(note(sharing.message, "danger"));
  const applying = sharing.pending?.kind === "apply" || isSyncing(sharing);
  children.push(
    switchRow("Start MonHop when you log in", {
      checked: autostart.view.enabled,
      description: autostartDescription(platform),
      disabled: applying || autostart.pending,
      id: "setup-autostart",
      onChange: actions.setAutostart,
    }),
  );
  return {
    element: card({
      title: "Displays",
      description:
        "Each computer starts with the layout from its own system settings. Switch to placing displays one by one when that is not how they sit on the desk.",
      children,
      actions: [
        button(applying ? "Applying…" : "Apply on both computers", {
          busy: applying,
          disabled: applying || busy || !canApplySetup(sharing),
          focusKey: "displays-apply",
          onClick: actions.applySetup,
        }),
      ],
    }),
    mounted,
  };
}

// The app rebuilds this screen on every status poll; keeping the editor alive keeps a drag alive with it.
let live = null;

function liveEditor(ctx, reset, arrangement) {
  const { sharing, actions, busy } = ctx;
  const view = sharing.view;
  const localSource = sharing.source === "local";
  const localLabel = `${platformLabel(view.localPlatform)} · This computer`;
  const setup = {
    sourceSide: sharing.source,
    sourcePlatform: localSource ? view.localPlatform : view.peerPlatform,
    destinationPlatform: localSource ? view.peerPlatform : view.localPlatform,
    localPlatform: view.localPlatform,
    sourceLabel: localSource ? localLabel : ctx.peerName,
    destinationLabel: localSource ? ctx.peerName : localLabel,
  };
  const key = Object.values(setup).join("|");
  const handlers = {
    onCommit: (placement, moving) => actions.commitArrangement(placement, moving),
    onReset: reset.apply,
    onMode: (mode) => actions.chooseArrangementMode(mode),
    onShowMonitor: (monitor, side) => actions.showMonitorOn(monitor, side),
    onUseDisplay: (id, inUse) => actions.useDisplay(id, inUse),
  };
  const shared = sharedMonitorChoices(sharing);
  const inUse = displayUseChoices(sharing);
  if (live?.key !== key) {
    releaseEditor();
    live = {
      key,
      editor: createArrangementView({
        ...setup,
        ...handlers,
        arrangement,
        shared,
        inUse,
        disabled: busy,
        canReset: reset.enabled,
        resetHint: reset.hint,
      }),
      handle: null,
    };
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

function arrangementsDisclosure(ctx) {
  const { sharing } = ctx;
  const entries = sharing.arrangements;
  if (pendingDelete && !entries.some((entry) => entry.name === pendingDelete)) pendingDelete = null;
  const content = [];
  if (entries.length) content.push(rows(entries.map((entry) => arrangementRow(ctx, entry))));
  else
    content.push(
      note(
        "MonHop remembers each arrangement you apply, for its display configuration. You can also save one under a name to switch between several later.",
      ),
    );
  content.push(saveForm(ctx));
  const label = entries.length ? `Saved arrangements (${entries.length})` : "Saved arrangements";
  return createAccordion("saved-arrangements", "saved-arrangements", label, ...content);
}

function arrangementRow(ctx, entry) {
  const { sharing, actions, busy, peerName } = ctx;
  const inputName = entry.sourceSide === "local" ? "this computer" : peerName;
  const crossings = `${entry.crossings} crossing${entry.crossings === 1 ? "" : "s"}`;
  const fit = entry.layout ? "" : " · Does not fit the current displays";
  const detail = `${MODE_INFO[entry.mode].shortLabel} · ${crossings} · Input: ${inputName}${fit}`;
  const key = rowKey(entry.name);
  const deleting = pendingDelete === entry.name;
  const remove = button(deleting ? "Confirm delete" : "Delete", {
    variant: deleting ? "destructive" : "ghost",
    size: "sm",
    disabled: busy,
    focusKey: `${key}-delete`,
    pressed: deleting,
    onClick: () => {
      if (pendingDelete !== entry.name) {
        pendingDelete = entry.name;
        remove.textContent = "Confirm delete";
        remove.dataset.variant = "destructive";
        remove.setAttribute("aria-pressed", "true");
        return;
      }
      pendingDelete = null;
      void actions.deleteArrangement(entry.name);
    },
  });
  return row({
    title: entry.name,
    detail,
    leading: entry.automatic ? statusChip({ tone: "neutral", label: "Remembered" }) : null,
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
