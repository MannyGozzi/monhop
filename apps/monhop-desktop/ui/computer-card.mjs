import { ACCORDION_TOGGLE, createAccordion } from "./accordion.mjs";
import { computerStatus } from "./computer-status.mjs";
import { computerArrangements, displayName } from "./computers-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import { isConnected } from "./sharing-model.mjs";
import {
  displayChipLabel,
  displayStripSides,
  hasDisplayStrip,
  layoutRows,
} from "./computer-card-model.mjs";
import {
  badge,
  button,
  card,
  el,
  icon,
  note,
  platformGlyph,
  presence,
  row,
  rows,
  statusChip,
  swap,
  switchRow,
} from "./dom.mjs";

// One computer rendered one way, so Home and the Setup list can never disagree about it.
// The name is its own editor and the status and switch sit in the header, so the card is one row.
export function computerCard(ctx, computer, { scope, extras = [], details = [] } = {}) {
  const { busy, renaming, renamePending, active } = ctx;
  const fingerprint = computer.fingerprint;
  const name = displayName(computer);
  const status = computerStatus(computer, ctx.sharing.view, active);
  const inUse = fingerprint === active;
  const pending = renamePending === fingerprint;
  const editing = renaming === fingerprint || pending;
  const key = `${scope}-${fingerprint}`;

  const children = [
    el("div", {
      className: "computer-card",
      children: [
        platformGlyph(computer.platform),
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
        el("div", {
          className: "computer-actions",
          children: [
            swap(`${key}-status`, statusChip(status), `${status.tone}|${status.label}`),
            swap(
              `${key}-switch`,
              useSwitch(ctx, fingerprint, scope, inUse),
              inUse ? "pause" : "use",
            ),
          ],
        }),
      ],
    }),
    swap(`${key}-detail`, note(status.detail), status.detail, { block: true }),
    presence(`${key}-displays`, displayStrip(computer)),
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
  if (busy) node.dataset.busy = "true";
  return node;
}

function useSwitch(ctx, fingerprint, scope, inUse) {
  const { actions, busy } = ctx;
  return inUse
    ? button("Pause", {
        variant: "outline",
        size: "sm",
        iconName: "power-off",
        disabled: busy,
        focusKey: `${scope}-pause-${fingerprint}`,
        onClick: () => actions.useComputer(null),
      })
    : button("Use", {
        size: "sm",
        iconName: "keyboard",
        disabled: busy,
        focusKey: `${scope}-use-${fingerprint}`,
        onClick: () => actions.useComputer(fingerprint),
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

// --- display strip ---------------------------------------------------------

// Each side shows its live displays when the link is reporting them and the last saved ones
// otherwise, so the card is never blank and never claims a stale list is current.
function displayStrip(computer) {
  const sides = displayStripSides(computer.setup);
  if (!hasDisplayStrip(sides)) return null;
  return el("div", {
    className: "computer-displays",
    children: [
      displayStripRow("This computer", sides.local),
      displayStripRow(displayName(computer), sides.peer),
    ],
  });
}

function displayStripRow(title, side) {
  if (!side.displays.length) return null;
  return el("div", {
    className: "display-strip-row",
    children: [
      el("span", {
        className: "display-strip-label",
        children: [
          el("span", { text: title }),
          side.lastSeen
            ? el("span", { className: "computer-displays-note", text: "Last seen" })
            : null,
        ],
      }),
      el("div", {
        className: "display-strip-chips",
        children: side.displays.map((item) => displayChip(item)),
      }),
    ],
  });
}

// The label is its own element so a long monitor name is cut with an ellipsis inside the chip.
function displayChip(display) {
  const chip = badge("", "secondary", "monitor");
  chip.append(el("span", { className: "display-chip-text", text: displayChipLabel(display) }));
  return chip;
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
  else content.push(rows(entries.map((item) => layoutRow(ctx, fingerprint, item))));
  const label = entries.length ? `Layouts (${entries.length})` : "Layouts";
  const accordion = createAccordion(`${key}-layouts`, "computer-layouts", label, ...content);
  // The manager toggles the panel first and announces it afterwards, so reading the list here
  // re-renders the card without the press being lost.
  accordion.addEventListener(ACCORDION_TOGGLE, (event) => {
    if (event.detail.open) void actions.loadComputerArrangements(fingerprint);
  });
  return accordion;
}

function layoutRow(ctx, fingerprint, item) {
  const { actions } = ctx;
  const { entry, key, armed, disabled, load } = item;
  const chips = [
    statusChip({ tone: "neutral", label: entry.automatic ? "Remembered" : "Saved" }),
    entry.fits ? statusChip({ tone: "connected", label: "Fits now" }) : null,
  ].filter(Boolean);
  return row({
    title: entry.name,
    detail: `${entry.crossings} crossing${entry.crossings === 1 ? "" : "s"}`,
    leading: el("span", { className: "row-leading-chips", children: chips }),
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
