import { createAccordion } from "./accordion.mjs";
import { computerStatus } from "./computer-status.mjs";
import { displayName } from "./computers-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import {
  button,
  card,
  el,
  icon,
  note,
  platformGlyph,
  presence,
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
    presence(
      `${key}-extras`,
      extras.some(Boolean)
        ? el("div", { className: "card-extras", children: extras.filter(Boolean) })
        : null,
    ),
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
