import { ACCORDION_TOGGLE, createAccordion, presence } from "./accordion.mjs";
import { computerStatus } from "./computer-status.mjs";
import { computerArrangements, displayName } from "./computers-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import {
  displayNoticeCopy,
  isConnected,
  isSessionActive,
  noticePresentation,
  releaseToShareCopy,
} from "./sharing-model.mjs";
import { DRAWN_DISPLAYS_NOTE } from "./dashboard-arrangement.mjs";
import { normalizeClipboardView } from "./clipboard-model.mjs";
import { clipboardLine } from "./studio-model.mjs";
import { inspectorHeader } from "./screen-ready.mjs";
import { pairBadgeLabel } from "./pair-badge-model.mjs";
import {
  computerMeta,
  computerTone,
  controlSwitchRows,
  groupedFingerprint,
  isComputerEnabled,
  isInFlight,
  isLinkedWith,
  layoutChips,
  layoutRows,
  resolveEnabledList,
  shareSwitchLabel,
  toggleComputerEnabled,
} from "./computer-card-model.mjs";
import {
  button,
  copyFeedbackControls,
  el,
  icon,
  motionMs,
  note,
  pairBadge,
  platformGlyph,
  rows,
  sinceChanged,
  stableLabel,
  statusChip,
  swap,
  switchRow,
  withTooltip,
} from "./dom.mjs";

// A saved arrangements list rebuilt within this window is still the same entrance, stagger included.
const LIST_ENTER_MS = 600;

// A computer's inspector: who it is, its state with the one switch that shares with it, the
// directions of control, its clipboard, its arrangement and saved arrangements, its details and Forget.
export function computerInspector(ctx, computer) {
  const { active } = ctx;
  const fingerprint = computer.fingerprint;
  const name = displayName(computer);
  const local = localName(ctx);
  const status = computerStatus(computer, ctx.sharing.view, active, local);
  const enabledList = resolveEnabledList(ctx.sharing.view, ctx.computers, active);
  const enabled = isComputerEnabled(fingerprint, enabledList);
  // A paused group stays switched on but shares with nothing, so its switches read off.
  const on = enabled && !ctx.studio.paused;
  const tone = on ? computerTone(fingerprint, enabledList) : "idle";
  const key = `inspector-${fingerprint}`;
  const node = el("div", {
    className: "inspector-view computer-inspector",
    dataset: { kind: "computer", tone: status.tone },
    children: [
      header(ctx, computer, name, tone, key),
      stateBlock(ctx, computer, { name, status, enabled, on, key }),
      presence(`${key}-control`, enabled ? directions(ctx, computer, name, local) : null),
      presence(`${key}-clipboard`, clipboardSection(ctx, computer, name, key)),
      arrangementSection(ctx, computer, key),
      details(ctx, computer, key),
      forgetSection(ctx, computer, name, key),
    ],
  });
  if (ctx.busy) node.dataset.busy = "true";
  return node;
}

function section(title, children, { actions = [], className = "" } = {}) {
  return el("section", {
    className: ["inspector-section", className].filter(Boolean).join(" "),
    children: [
      el("div", {
        className: "inspector-section-heading",
        children: [
          el("h3", { text: title }),
          actions.length ? el("div", { className: "row-actions", children: actions }) : null,
        ],
      }),
      ...children,
    ],
  });
}

// Text that changes in place: the words cross-fade and the block glides to its new height. Settings
// draws its status lines with it too.
export function fadingText(key, className, value, tone = null) {
  if (!value) return presence(key, null);
  const node = el("p", { className, text: value, dataset: tone ? { tone } : undefined });
  return presence(key, swap(`${key}-text`, node, `${tone}|${value}`, { block: true }));
}

// A small line under the state: a glyph, then what it means for this computer.
function inlineNote(value, tone, glyph, live = false) {
  return el("p", {
    className: "inspector-note",
    dataset: { tone },
    attrs: live ? { "aria-live": "polite" } : undefined,
    children: [
      el("span", {
        className: "inspector-note-glyph",
        attrs: { "aria-hidden": "true" },
        children: [glyph],
      }),
      el("span", { text: value }),
    ],
  });
}

// --- header -------------------------------------------------------------------

// The tile, the name that renames in place, what and where it is, and the pairing picture.
function header(ctx, computer, name, tone, key) {
  const fingerprint = computer.fingerprint;
  const pending = ctx.renamePending === fingerprint;
  const editing = ctx.renaming === fingerprint || pending;
  const meta = editing
    ? pending
      ? "Saving…"
      : "Enter to save · Esc to cancel"
    : computerMeta(platformLabel(computer.platform), computer.address);
  return inspectorHeader({
    glyph: platformGlyph(computer.platform),
    tone,
    titleNode: swap(
      `${key}-name`,
      editing ? nameField(ctx, computer, name, pending) : nameButton(ctx, computer, name),
      editing ? "edit" : "view",
    ),
    metaNode: el("div", {
      className: "computer-subtitle",
      children: [
        swap(`${key}-meta`, el("p", { className: "inspector-meta", text: meta }), meta),
        computer.badge ? pairPicture(computer, key) : null,
      ],
    }),
  });
}

// The other computer shows this same picture in its own inspector for this one.
function pairPicture(computer, key) {
  const node = el("span", {
    className: "pair-picture",
    attrs: { tabindex: 0 },
    dataset: { focusKey: `${key}-pair-picture` },
    children: [
      el("span", { className: "pair-picture-label", text: "Pairing picture" }),
      pairBadge(computer.badge),
    ],
  });
  return withTooltip(
    node,
    `${key}-pair-picture-tooltip`,
    `${pairBadgeLabel(computer.badge)}. The other computer shows this same picture. If they differ, forget this computer and pair again.`,
  );
}

// The name reads as text and edits in place: a pencil appears on hover and focus, and the line
// below it says how to finish.
function nameButton(ctx, computer, name) {
  const node = el("button", {
    className: "computer-name",
    attrs: { type: "button", title: "Click to rename", "aria-label": `Rename ${name}` },
    children: [el("span", { text: name }), icon("pencil", 12)],
  });
  node.dataset.focusKey = `inspector-name-${computer.fingerprint}`;
  node.addEventListener("click", () => ctx.actions.startRename(computer.fingerprint));
  return node;
}

function nameField(ctx, computer, name, pending) {
  const { actions } = ctx;
  const field = el("input", {
    className: "computer-name-input",
    attrs: {
      id: `rename-field-${computer.fingerprint}`,
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
  field.dataset.focusKey = `rename-field-${computer.fingerprint}`;
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

// --- state ----------------------------------------------------------------------

// The state: the label with its tone dot, a spinner while it is on its way, and what the state
// means. The toolbar's button starts, pauses and resumes sharing; once two or more computers are
// paired, a switch here picks which of them share.
function stateBlock(ctx, computer, { name, status, enabled, on, key }) {
  const release =
    enabled && status.key === "sharing"
      ? releaseToShareCopy(ctx.sharing.view?.blockingPresses)
      : "";
  const spinning = isInFlight(status.key);
  return el("section", {
    className: "state-block computer-state",
    dataset: { tone: status.tone },
    children: [
      el("div", {
        className: "computer-state-heading",
        children: [
          el("span", {
            className: "status-dot computer-state-dot",
            dataset: { tone: status.tone },
            attrs: { "aria-hidden": "true" },
          }),
          el("strong", {
            className: "computer-state-label",
            children: [
              swap(`${key}-state-label`, el("span", { text: status.label }), status.label),
            ],
          }),
          swap(
            `${key}-state-spinner`,
            el("span", {
              className: "computer-state-spinner",
              attrs: { "aria-hidden": "true" },
              children: [spinning ? el("span", { className: "spinner" }) : null],
            }),
            spinning ? "spin" : "rest",
          ),
          ctx.computers.items.length > 1 ? shareSwitch(ctx, computer, name, on) : null,
        ],
      }),
      fadingText(`${key}-state-detail`, "computer-state-detail", status.detail),
      presence(`${key}-release`, release ? inlineNote(release, "warning", icon("keyboard")) : null),
      presence(`${key}-notice`, noticeLine(ctx, computer, enabled)),
    ],
  });
}

// The kit's switch, lifted out of its row: the state above is its label, so the whole block is not
// one click target. On means sharing with this computer; turning it on also resumes a pause.
function shareSwitch(ctx, computer, name, on) {
  const label = shareSwitchLabel(name);
  const control = switchRow(label, {
    checked: on,
    disabled: ctx.busy,
    focusKey: `share-${computer.fingerprint}`,
    onChange: () => toggleComputerEnabled(ctx.actions, computer.fingerprint, on),
  }).querySelector(".switch");
  control.classList.add("computer-share-switch");
  control.setAttribute("aria-label", label);
  if (ctx.busy) control.setAttribute("aria-busy", "true");
  return control;
}

// A display change MonHop is already settling says so here; only one the user has to act on earns
// a banner above the canvas.
function noticeLine(ctx, computer, enabled) {
  const notice = ctx.sharing.view?.displayNotice;
  if (!enabled || !notice || noticePresentation(notice.kind) !== "inline") return null;
  const copy = displayNoticeCopy(notice.kind, displayName(computer));
  return inlineNote(copy.body, "notice", el("span", { className: "notice-dot" }), true);
}

// --- directions -----------------------------------------------------------------

// MonHop is always bidirectional: each direction of control is its own switch, keyed to its own
// entry in the group's control record. The last one on cannot be turned off; Pause is how to stop.
function directions(ctx, computer, peerName, local) {
  const { actions, sharing } = ctx;
  const syncing = sharing.pending?.kind === "control" || sharing.view?.control?.syncing === true;
  const controls = controlSwitchRows(
    sharing.view?.control,
    local,
    peerName,
    syncing,
    computer.fingerprint,
  );
  const switches = controls.map((control) => {
    const focusKey = `control-${computer.fingerprint}-${control.direction}`;
    const row = switchRow(control.label, {
      description: control.hint,
      checked: control.checked,
      disabled: control.disabled,
      focusKey,
      onChange: (checked) => actions.setControl(computer.fingerprint, control.direction, checked),
    });
    enterHint(row, focusKey, control.hint);
    return row;
  });
  return section("Directions", [rows(switches)], { className: "computer-directions" });
}

// A hint that just came or changed fades in, carried on across the renders meanwhile.
function enterHint(row, key, hint) {
  const small = row.querySelector(".switch-copy > small");
  const elapsed = sinceChanged(`hint-${key}`, hint);
  if (!small || elapsed >= motionMs("--motion-base")) return;
  small.dataset.enter = "true";
  small.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
}

// --- clipboard ------------------------------------------------------------------

// The switch lives in Settings; here is what it means for this one computer, and the way there.
function clipboardSection(ctx, computer, name, key) {
  if (!ctx.clipboard) return null;
  const view = normalizeClipboardView(ctx.clipboard.view);
  const link = el("button", {
    className: "inspector-link",
    text: "Clipboard settings",
    attrs: { type: "button" },
    dataset: { focusKey: `${key}-clipboard-settings` },
  });
  link.addEventListener("click", () => ctx.actions.openSettingsPage("clipboard"));
  return section(
    "Clipboard",
    [fadingText(`${key}-clipboard-line`, "note", clipboardLine(view, computer.fingerprint, name))],
    { actions: [link], className: "computer-clipboard" },
  );
}

// --- arrangement and saved arrangements -------------------------------------------

// The canvas draws the displays; here is the way into Arrange and the arrangements kept with this computer.
// Arrange itself floats over the canvas; the saved arrangements to load stay here.
function arrangementSection(ctx, computer, key) {
  return section("Arrangement", [layoutsDisclosure(ctx, computer, key)], {
    className: "computer-arrangement",
  });
}

function layoutsDisclosure(ctx, computer, key) {
  const { actions, active, sharing, busy, layoutForget } = ctx;
  const fingerprint = computer.fingerprint;
  const store = computerArrangements(ctx.computerArrangements, fingerprint);
  const entries = layoutRows({
    fingerprint,
    entries: store.items,
    armed: layoutForget,
    linked: isLinkedWith(fingerprint, {
      connected: isConnected(sharing),
      peerFingerprint: sharing.view?.peerFingerprint,
      active,
    }),
    canArrange: ctx.studio.canArrange,
    sessionActive: isSessionActive(sharing),
    busy,
    pending: store.loading,
  });
  const content = [];
  if (store.error) content.push(note(store.error, "danger"));
  if (!entries.length)
    content.push(
      note(
        store.loading
          ? "Reading the saved arrangements…"
          : "MonHop remembers each arrangement you apply with this computer.",
      ),
    );
  else content.push(layoutList(ctx, fingerprint, entries));
  const label = entries.length ? `Saved arrangements (${entries.length})` : "Saved arrangements";
  const accordion = createAccordion(`${key}-layouts`, "computer-layouts", label, ...content);
  // The manager toggles the panel first and announces it afterwards, so reading the list here
  // re-renders the inspector without the press being lost.
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
    // The negative delay resumes the entrance, stagger included, so a status poll part way
    // through it does not send every row back to the start.
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

// The name over one quiet line, its crossings then its chips, with Load and Delete beside it.
function layoutRow(ctx, fingerprint, item, index) {
  const { actions } = ctx;
  const { entry, key, armed, disabled, load } = item;
  const node = el("div", {
    className: "row layout-row",
    children: [
      el("div", {
        className: "row-copy",
        children: [
          el("strong", { text: entry.name, attrs: { title: entry.name } }),
          el("span", {
            className: "layout-row-meta",
            children: [
              el("span", {
                text: `${entry.crossings} crossing${entry.crossings === 1 ? "" : "s"}`,
              }),
              layoutChipStrip(`layout-${key}`, entry),
            ],
          }),
        ],
      }),
      el("div", {
        className: "row-actions",
        children: [
          loadButton(actions, fingerprint, entry, key, load, disabled),
          forgetLayoutButton(actions, fingerprint, entry, key, armed, disabled),
        ],
      }),
    ],
  });
  node.style.setProperty("--row-index", String(index));
  return node;
}

// Arming keeps the button's width, so Load beside it stays put while the label turns into the question.
function forgetLayoutButton(actions, fingerprint, entry, key, armed, disabled) {
  const node = button("", {
    variant: armed ? "destructive" : "ghost",
    size: "sm",
    disabled,
    pressed: armed,
    focusKey: `layout-forget-${key}`,
    onClick: () => actions.pressLayoutForget(fingerprint, entry.name),
  });
  node.append(stableLabel(`layout-forget-${key}`, ["Delete", "Confirm delete"], armed ? 1 : 0));
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
  if (load.reason || load.hint) node.title = load.reason || load.hint;
  return node;
}

// --- details --------------------------------------------------------------------

// The fingerprint to compare aloud, the last drop with Copy (never in red), and how the canvas
// drew the displays when it drew them from what each computer last reported.
function details(ctx, computer, key) {
  const fingerprint = computer.fingerprint;
  const drawn = !ctx.arrange.active && computer.setup?.saved === true;
  return createAccordion(
    `inspector-computer-${fingerprint}`,
    "computer-details",
    "Details",
    fact(
      "Fingerprint",
      el("code", { className: "computer-fingerprint", text: groupedFingerprint(fingerprint) }),
    ),
    fingerprint === ctx.active ? presence(`${key}-last-drop`, lastDrop(ctx)) : null,
    drawn ? note(DRAWN_DISPLAYS_NOTE) : null,
  );
}

function fact(label, ...children) {
  return el("div", {
    className: "computer-fact",
    children: [el("span", { className: "computer-fact-label", text: label }), ...children],
  });
}

function lastDrop(ctx) {
  const failure = ctx.sharing.view?.lastFailure;
  if (!failure) return null;
  const feedback = ctx.dropCopyFeedback;
  const { line } = copyFeedbackControls(feedback, {});
  return fact(
    "Last connection drop",
    el("p", { className: "computer-drop-text", text: failure }),
    el("div", {
      className: "computer-drop-actions",
      children: [
        copyButton(ctx, feedback),
        swap("inspector-copy-feedback", line, `${feedback.state}|${feedback.message}`, {
          block: true,
        }),
      ],
    }),
  );
}

// Copy turns into Copied in place: the glyph and the word cross-fade, the width holds.
function copyButton(ctx, feedback) {
  const copied = feedback.state === "success";
  const node = button("", {
    variant: "outline",
    size: "sm",
    disabled: ctx.busy || feedback.state === "pending",
    focusKey: "inspector-copy-last-drop",
    onClick: ctx.actions.copyLastDrop,
  });
  node.append(
    swap("inspector-copy-glyph", icon(copied ? "check" : "copy"), copied ? "check" : "copy"),
    stableLabel("inspector-copy-last-drop", ["Copy", "Copied"], copied ? 1 : 0),
  );
  return node;
}

// --- forget ---------------------------------------------------------------------

// Two presses in two places: the first opens the question above it, the second answers it there,
// so a double click can never forget a computer.
function forgetSection(ctx, computer, name, key) {
  const { actions, busy, forgetConfirmed } = ctx;
  const fingerprint = computer.fingerprint;
  const armed = forgetConfirmed === fingerprint;
  const arm = button("Forget this computer", {
    variant: "ghost",
    size: "sm",
    iconName: "trash-2",
    pressed: armed,
    disabled: busy,
    focusKey: `forget-confirm-${fingerprint}`,
    onClick: () => actions.confirmForget(fingerprint, !armed),
  });
  arm.classList.add("computer-forget-arm");
  arm.setAttribute("aria-expanded", String(armed));
  const question = armed
    ? el("div", {
        className: "computer-forget-question",
        children: [
          el("p", {
            className: "computer-forget-title",
            text: `Forget ${name}?`,
          }),
          note("Removes the pairing. This computer keeps its own identity."),
          el("div", {
            className: "computer-forget-actions",
            children: [
              button("Keep", {
                variant: "secondary",
                size: "sm",
                disabled: busy,
                focusKey: `forget-keep-${fingerprint}`,
                onClick: () => actions.confirmForget(fingerprint, false),
              }),
              button("Forget computer", {
                variant: "destructive",
                size: "sm",
                iconName: "trash-2",
                disabled: busy,
                focusKey: `forget-${fingerprint}`,
                onClick: () => actions.forgetComputer(fingerprint),
              }),
            ],
          }),
        ],
      })
    : null;
  return el("section", {
    className: "inspector-section computer-forget",
    children: [presence(`${key}-forget`, question), arm],
  });
}
