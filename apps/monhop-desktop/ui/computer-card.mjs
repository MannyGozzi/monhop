import { ACCORDION_TOGGLE, createAccordion } from "./accordion.mjs";
import { computerStatus } from "./computer-status.mjs";
import { computerArrangements, displayName } from "./computers-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import { displayNoticeCopy, isConnected, noticePresentation } from "./sharing-model.mjs";
import { createDashboardArrangement } from "./dashboard-arrangement.mjs";
import {
  controlSwitchRows,
  displaysFreshness,
  layoutChips,
  layoutRows,
} from "./computer-card-model.mjs";
import {
  button,
  card,
  el,
  icon,
  iconButton,
  note,
  platformGlyph,
  presence,
  row,
  rows,
  sinceChanged,
  swap,
  switchRow,
} from "./dom.mjs";

// The longest of the play/stop animations. A card rebuilt inside this window starts its motion
// where the last one left off, so a status poll mid-morph does not replay it from the top.
const USE_MOTION_MS = 700;
// A Layouts list rebuilt within this window is still the same entrance, stagger included.
const LIST_ENTER_MS = 600;

// One computer rendering keeps Home and Set up in sync while each page owns its own actions.
export function computerCard(
  ctx,
  computer,
  { scope, extras = [], details = [], viewport = true } = {},
) {
  const { busy, renaming, renamePending, active } = ctx;
  const fingerprint = computer.fingerprint;
  const name = displayName(computer);
  const local = localName(ctx);
  const status = computerStatus(computer, ctx.sharing.view, active, local);
  const inUse = fingerprint === active;
  const pending = renamePending === fingerprint;
  const editing = renaming === fingerprint || pending;
  const key = `${scope}-${fingerprint}`;

  const children = [
    el("div", {
      className: "computer-card",
      children: [
        el("div", {
          className: "computer-identity",
          children: [
            el("span", {
              className: "computer-icon-wrap",
              children: [platformGlyph(computer.platform)],
            }),
            el("div", {
              className: "computer-copy",
              children: [
                swap(
                  `${key}-name`,
                  editing
                    ? nameField(ctx, computer, scope, name, pending)
                    : nameButton(ctx, computer, scope, name),
                  editing ? "edit" : "view",
                ),
                el("span", {
                  className: "computer-meta",
                  text: editing
                    ? pending
                      ? "Saving…"
                      : "Enter to save · Esc to cancel"
                    : [platformLabel(computer.platform), computer.address].filter(Boolean).join(" · "),
                }),
              ],
            }),
          ],
        }),
        el("div", {
          className: "computer-actions",
          children: [
            scope === "home"
              ? sharingPill(ctx, fingerprint, name, inUse)
              : useToggle(ctx, fingerprint, scope, inUse),
          ],
        }),
      ],
    }),
    presence(
      `${key}-detail`,
      scope === "home" && inUse && status.key === "sharing" ? note(status.detail) : null,
    ),
    presence(
      `${key}-control`,
      scope === "home" && inUse ? controlSwitches(ctx, computer, name, local) : null,
    ),
    presence(`${key}-notice`, noticeLine(ctx, computer, inUse)),
    presence(`${key}-viewport`, viewport ? cardArrangement(ctx, computer, key) : null),
    presence(
      `${key}-extras`,
      extras.some(Boolean)
        ? el("div", { className: "card-extras", children: extras.filter(Boolean) })
        : null,
    ),
    layoutsDisclosure(ctx, computer, key),
    createAccordion(
      `${scope}-computer-${fingerprint}`,
      "computer-details",
      "Details",
      ...details,
      ...forgetControls(ctx, computer, name),
    ),
  ];
  const node = card({ children: children.filter(Boolean), tone: status.tone });
  const identity = node.querySelector(".computer-identity");
  if (inUse && ["home", "setup"].includes(scope) && identity)
    identity.dataset.sharedTransition = "active-computer-identity";
  if (scope === "home" && inUse) node.classList.add("home-hero");
  if (busy) node.dataset.busy = "true";
  return node;
}

function sharingPill(ctx, fingerprint, name, inUse) {
  const { actions, busy } = ctx;
  const state = inUse ? "active" : "idle";
  const label = inUse ? "Pause sharing" : "Start sharing";
  const node = el("button", {
    className: "sharing-pill",
    attrs: {
      type: "button",
      "aria-label": `${label} with ${name}`,
      "aria-pressed": String(inUse),
      "aria-busy": busy ? "true" : null,
    },
    dataset: { state, busy: String(busy) },
    children: [
      el("span", {
        className: "sharing-pill-surfaces",
        attrs: { "aria-hidden": "true" },
        children: [
          el("span", { className: "sharing-pill-surface idle" }),
          el("span", { className: "sharing-pill-surface active" }),
        ],
      }),
      el("span", {
        className: "sharing-pill-icon",
        attrs: { "aria-hidden": "true" },
        children: [
          sharingGlyph("play", !inUse),
          sharingGlyph("square", inUse),
          el("span", { className: "sharing-pill-live-dot", attrs: { "aria-hidden": "true" } }),
        ],
      }),
      el("span", {
        className: "sharing-pill-label",
        attrs: { "aria-hidden": "true" },
        children: [
          el("span", { text: "Start sharing", dataset: { current: String(!inUse) } }),
          el("span", { text: "Sharing", dataset: { current: String(inUse) } }),
          el("span", { className: "sharing-pill-pause", text: "Pause", attrs: { "aria-hidden": "true" } }),
        ],
      }),
      el("span", { className: "sharing-pill-spinner", attrs: { "aria-hidden": "true" } }),
    ],
  });
  node.disabled = busy;
  node.addEventListener("click", () => actions.useComputer(inUse ? null : fingerprint));
  const elapsed = sinceChanged(`home-sharing-state-${fingerprint}`, state);
  if (elapsed < USE_MOTION_MS) {
    node.dataset.enter = "true";
    node.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
  }
  return node;
}

function sharingGlyph(name, current) {
  return el("span", {
    className: "sharing-pill-glyph",
    dataset: { current: String(current) },
    children: [icon(name, 15)],
  });
}

// One button for both states, so pressing it keeps the focus and the glyph morphs in place
// instead of one control being swapped for another.
function useToggle(ctx, fingerprint, scope, inUse) {
  const { actions, busy } = ctx;
  const state = inUse ? "sharing" : "idle";
  const node = iconButton({
    id: `${scope}-use-${fingerprint}`,
    label: inUse ? "Pause sharing with this computer" : "Use this computer",
    art: useGlyphs(inUse),
    size: "sm",
    disabled: busy,
    onClick: () => actions.useComputer(inUse ? null : fingerprint),
  });
  node.classList.add("use-toggle");
  node.dataset.state = state;
  const elapsed = sinceChanged(`${scope}-use-state-${fingerprint}`, state);
  if (elapsed < USE_MOTION_MS) {
    node.dataset.enter = "true";
    node.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
  }
  return node;
}

// Both glyphs are always drawn and the current one is marked, so CSS alone cross-fades and turns
// one into the other; the fill and the ring are the button's surface under them.
function useGlyphs(inUse) {
  return el("span", {
    className: "use-glyphs",
    attrs: { "aria-hidden": "true" },
    children: [
      el("span", { className: "use-fill" }),
      el("span", { className: "use-ring" }),
      useGlyph("play", !inUse),
      useGlyph("square", inUse),
    ],
  });
}

function useGlyph(name, current) {
  return el("span", {
    className: "use-glyph",
    dataset: { current: String(current) },
    children: [icon(name, 14)],
  });
}

// The name reads as text and edits in place: a pencil appears on hover and focus, and the
// hint line below says how to finish.
function nameButton(ctx, computer, scope, name) {
  const node = el("button", {
    className: "computer-name",
    attrs: { type: "button", title: "Click to rename", "aria-label": `Rename ${name}` },
    children: [el("span", { text: name }), icon("pencil", 12)],
  });
  node.dataset.focusKey = `${scope}-name-${computer.fingerprint}`;
  node.addEventListener("click", () => ctx.actions.startRename(computer.fingerprint, scope));
  return node;
}

function nameField(ctx, computer, scope, name, pending) {
  const { actions } = ctx;
  const field = el("input", {
    className: "computer-name-input",
    attrs: {
      id: `${scope}-rename-field-${computer.fingerprint}`,
      maxlength: 48,
      placeholder: name,
      "aria-label": `Name for ${name}`,
      autocomplete: "off",
      spellcheck: "false",
    },
  });
  field.value = ctx.renameDraft ?? computer.name;
  field.addEventListener("input", () => actions.draftRename(field.value));
  field.disabled = pending;
  field.dataset.focusKey = `${scope}-rename-field-${computer.fingerprint}`;
  // Enter and Escape settle the edit themselves; the blur they cause must not settle it again.
  let settled = pending;
  const commit = () => {
    if (settled) return;
    settled = true;
    const next = field.value.trim();
    if (next && next !== computer.name) actions.renameComputer(computer.fingerprint, next);
    else actions.cancelRename();
  };
  field.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      commit();
    } else if (event.key === "Escape") {
      event.preventDefault();
      settled = true;
      actions.cancelRename();
    }
  });
  field.addEventListener("blur", commit);
  return field;
}

function localName(ctx) {
  return platformLabel(ctx.state.snapshot?.platform ?? ctx.platform, true);
}

// MonHop is always bidirectional: each direction of control is its own switch. Home shows both only
// for the computer in use, since that is the only pairing sharing input right now.
function controlSwitches(ctx, computer, peerName, local) {
  const { actions, sharing } = ctx;
  const syncing = sharing.pending?.kind === "control" || sharing.view?.control?.syncing === true;
  const rows = controlSwitchRows(sharing.view?.control, local, peerName, syncing);
  return el("div", {
    className: "control-switches",
    children: rows.map((row) =>
      switchRow(row.label, {
        description: row.hint,
        checked: row.checked,
        disabled: row.disabled,
        focusKey: `control-${computer.fingerprint}-${row.direction}`,
        onChange: (checked) => actions.setControl(computer.fingerprint, row.direction, checked),
      }),
    ),
  });
}

function forgetControls(ctx, computer, name) {
  const { actions, busy, forgetConfirmed } = ctx;
  const confirmed = forgetConfirmed === computer.fingerprint;
  return [
    note(`Fingerprint ${computer.fingerprint.slice(0, 16)}…`),
    switchRow(`Forget ${name}`, {
      description: "Removes the pairing. This computer keeps its own identity.",
      checked: confirmed,
      disabled: busy,
      focusKey: `forget-confirm-${computer.fingerprint}`,
      onChange: (checked) => actions.confirmForget(computer.fingerprint, checked),
    }),
    el("div", {
      className: "card-actions",
      attrs: { "data-align": "start" },
      children: [
        button("Forget computer", {
          variant: "destructive",
          size: "sm",
          iconName: "trash-2",
          disabled: !confirmed || busy,
          focusKey: `forget-${computer.fingerprint}`,
          onClick: () => actions.forgetComputer(computer.fingerprint),
        }),
      ],
    }),
  ];
}

// --- displays ---------------------------------------------------------------

// The picture is the only place a card names displays, so nothing is listed twice. Home's card for
// the computer in use draws its own bigger one and asks for this to be left out.
function cardArrangement(ctx, computer, key) {
  // A computer with nothing saved still says so: blank space under the header reads as a fault.
  if (computer.setup?.saved !== true)
    return note("No layout yet. Arrange the displays to start sharing.");
  const localPlatform = ctx.state.snapshot?.platform ?? ctx.platform;
  return el("div", {
    className: "computer-viewport",
    children: [
      createDashboardArrangement(computer.setup, {
        local: localName(ctx),
        peer: displayName(computer),
        localPlatform,
        peerPlatform: computer.platform,
        compact: true,
        caption: displaysFreshness(computer.setup),
        motionKey: `${key}-viewport`,
      }),
    ],
  });
}

// A display change MonHop is already settling says so on the card itself. Only a change the user
// has to act on is worth the banner Home puts above everything.
function noticeLine(ctx, computer, inUse) {
  const notice = ctx.sharing.view?.displayNotice;
  if (!inUse || !notice || noticePresentation(notice.kind) !== "inline") return null;
  const copy = displayNoticeCopy(notice.kind, displayName(computer));
  return el("p", {
    className: "notice-line",
    attrs: { "aria-live": "polite" },
    children: [
      el("span", { className: "notice-dot", attrs: { "aria-hidden": "true" } }),
      el("span", { text: copy.body }),
    ],
  });
}

// --- layout history ---------------------------------------------------------

function layoutsDisclosure(ctx, computer, key) {
  const { actions, active, sharing, busy, layoutForget } = ctx;
  const fingerprint = computer.fingerprint;
  const store = computerArrangements(ctx.computerArrangements, fingerprint);
  const entries = layoutRows({
    fingerprint,
    entries: store.items,
    armed: layoutForget,
    isActive: fingerprint === active,
    connected: isConnected(sharing),
    busy,
    pending: store.loading,
  });
  const content = [];
  if (store.error) content.push(note(store.error, "danger"));
  if (!entries.length)
    content.push(
      note(
        store.loading
          ? "Reading the saved layouts…"
          : "MonHop remembers each layout applied with this computer.",
      ),
    );
  else content.push(layoutList(ctx, fingerprint, entries));
  const label = entries.length ? `Layouts (${entries.length})` : "Layouts";
  const accordion = createAccordion(`${key}-layouts`, "computer-layouts", label, ...content);
  // The manager toggles the panel first and announces it afterwards, so reading the list here
  // re-renders the card without the press being lost.
  accordion.addEventListener(ACCORDION_TOGGLE, (event) => {
    if (event.detail.open) void actions.loadComputerArrangements(fingerprint);
  });
  return accordion;
}

// The list is rebuilt on every status poll, so the rows only play their entrance when the set of
// names actually changed; each row follows the one above it.
function layoutList(ctx, fingerprint, entries) {
  const list = rows(entries.map((item, index) => layoutRow(ctx, fingerprint, item, index)));
  const names = JSON.stringify(entries.map((item) => item.entry.name));
  const elapsed = sinceChanged(`${fingerprint}-layout-list`, names);
  if (elapsed < LIST_ENTER_MS) {
    list.dataset.enter = "true";
    // As the play/stop toggle does: the negative delay resumes the entrance, stagger included,
    // so a status poll part way through it does not send every row back to the start.
    list.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
  }
  return list;
}

// One chip strip for a saved layout, shared with the connected editor's list so the two can never
// disagree. "Fits now" sits in a keyed slot of its own, which fades and scales as it comes and goes.
export function layoutChipStrip(key, entry) {
  const chips = layoutChips(entry);
  return el("span", {
    className: "row-leading-chips",
    dataset: { empty: String(chips.marks.length === 0 && !chips.fits) },
    children: [
      ...chips.marks.map((chip) => statusChip(chip)),
      swap(
        `${key}-fits`,
        chips.fits ? statusChip(chips.fits) : el("span", { className: "chip-slot" }),
        chips.fits ? "fits" : "none",
      ),
    ],
  });
}

function layoutRow(ctx, fingerprint, item, index) {
  const { actions } = ctx;
  const { entry, key, armed, disabled, load } = item;
  const node = row({
    title: entry.name,
    detail: `${entry.crossings} crossing${entry.crossings === 1 ? "" : "s"}`,
    leading: layoutChipStrip(`layout-${key}`, entry),
    actions: [
      loadButton(actions, fingerprint, entry, key, load, disabled),
      button(armed ? "Confirm forget" : "Forget", {
        variant: armed ? "destructive" : "ghost",
        size: "sm",
        disabled,
        pressed: armed,
        focusKey: `layout-forget-${key}`,
        onClick: () => actions.pressLayoutForget(fingerprint, entry.name),
      }),
    ],
  });
  node.style.setProperty("--row-index", String(index));
  return node;
}

// Always drawn, so a row never changes shape as the connection comes and goes; the title says
// what is missing while it cannot be pressed.
function loadButton(actions, fingerprint, entry, key, load, disabled) {
  const node = button("Load", {
    variant: "outline",
    size: "sm",
    disabled: disabled || !load.enabled,
    focusKey: `layout-load-${key}`,
    onClick: () => void actions.loadComputerArrangement(fingerprint, entry.name),
  });
  if (load.reason) node.title = load.reason;
  return node;
}
