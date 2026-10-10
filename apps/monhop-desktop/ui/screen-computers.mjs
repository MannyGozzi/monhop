import {
  canStartPairing,
  canSubmitPairingCode,
  codeGroups,
  codeSymbols,
  codeTimeLeft,
  countdownProgress,
  formatCodeInput,
  formatCountdown,
  pairingErrorPresentation,
  pairingStep,
  platformLabel,
  spokenCode,
} from "./pairing-model.mjs";
import { computerCard } from "./computer-card.mjs";
import { createAccordion, presence } from "./accordion.mjs";
import {
  button,
  card,
  clear,
  el,
  icon,
  iconButton,
  note,
  pairBadge,
  sinceChanged,
  stateCard,
  swap,
} from "./dom.mjs";
import { pairBadgeLabel } from "./pair-badge-model.mjs";

const PROGRESS_COPY = {
  connecting: ["Connecting", "Reaching the other computer."],
  verifying: ["Checking the code", "Both computers confirm the same code."],
  saving: ["Saving the pairing", "Almost done."],
  stopping: ["Stopping", "Cleaning up."],
};
const CODE_PLACEHOLDER = "XXXX-XXXX-XXXX";
const CODE_EXPIRED = "This code expired. Show a new code.";
export const PAIRING_CODE_INPUT_ID = "pairing-code-input";
const CODE_ERROR_ID = "pairing-code-error";
export const COUNTDOWN_TEXT_ID = "pairing-countdown-text";
// A badge drawn again within this window is still the same entrance, stagger included.
const BADGE_ENTER_MS = 900;

export function renderComputers(nodes, ctx) {
  const { computers, actions, busy, gate, gates, pairingPending, pairingOperation, showPairing } =
    ctx;
  clear(nodes.computersContent);
  clear(nodes.computersActions);
  nodes.computersActions.append(
    iconButton({
      id: "pairing-reload",
      label: "Reload pairing",
      disabled: !gate.allowed || busy,
      busy: pairingPending && pairingOperation === "pairing_open",
      onClick: actions.openPairing,
    }),
  );
  for (const computer of computers.items)
    nodes.computersContent.append(computerCard(ctx, computer, { scope: "setup" }));
  // A locked section still shows what is already paired; only the exchange, which cannot run, is withheld.
  if (gates.computers.locked) {
    if (computers.items.length === 0) nodes.computersContent.append(addComputerIntro());
    return;
  }
  if (!showPairing) {
    nodes.computersContent.append(
      addComputerIntro([
        button("Add computer", {
          iconName: "link",
          disabled: busy,
          focusKey: "pairing-begin",
          onClick: actions.beginPairing,
        }),
      ]),
    );
    return;
  }
  renderExchange(nodes, ctx);
}

function addComputerIntro(actions) {
  return card({
    title: "Add a computer",
    description: "Show a short code on one computer and type it on the other.",
    actions,
  });
}

function renderExchange(nodes, ctx) {
  const { state, pairing } = ctx;
  const step = pairingStep(pairing, ctx.now);
  // Keyed off "nothing paired yet", so the badge's entrance plays the moment it first appears.
  const badgeElapsed = sinceChanged(
    "pairing-badge",
    step === "paired" ? pairing.view.peerFingerprint : null,
  );
  const content = stepContent(step, { ...ctx, badgeElapsed });
  const signature = step === "expired" ? "error" : step;
  if (content)
    nodes.computersContent.append(swap("pairing-step", content, signature, { block: true }));
  const message = presence(
    "pairing-message",
    pairing.message ? note(pairing.message, "danger") : null,
  );
  if (message) nodes.computersContent.append(message);
  if (state.snapshot?.platform === "macos") nodes.computersContent.append(localNetworkHelp(ctx));
}

function stepContent(step, ctx) {
  switch (step) {
    case "closed":
      return closedContent(ctx);
    case "identity":
      return identityMissing(ctx);
    case "choose":
      return chooseCard(ctx);
    case "enter":
      return enterCard(ctx);
    case "show":
      return showCard(ctx);
    case "expired":
      // Drawn as the error the backend reports on its next status, so that status changes nothing.
      return errorCard(ctx, pairingErrorPresentation({ role: "showing", message: CODE_EXPIRED }));
    case "progress":
      return progressCard(ctx);
    case "paired":
      return pairedCard(ctx);
    default:
      return errorCard(ctx, pairingErrorPresentation(ctx.pairing.view));
  }
}

function closedContent(ctx) {
  const { state, actions, busy, gate, pairingPending } = ctx;
  if (!gate.allowed)
    return card({ title: "Finish Get ready first", description: gate.detail, tone: "muted" });
  if (pairingPending)
    return stateCard({
      id: "pairing-state",
      tone: "checking",
      title: "Opening pairing",
      detail:
        state.snapshot?.platform === "macos"
          ? "Reading the saved identity. Approve the Keychain prompt if macOS shows one."
          : "Reading the saved identity.",
    });
  return card({
    title: "Add a computer",
    description: gate.detail,
    actions: [button("Open pairing", { disabled: busy, onClick: actions.openPairing })],
  });
}

function identityMissing(ctx) {
  const { pairing, actions, busy, gate } = ctx;
  return card({
    title: "Create this computer's identity",
    description:
      pairing.view.message ||
      "MonHop creates a private key once on this computer. It never leaves this computer.",
    actions: [
      button("Create identity", {
        disabled: !gate.allowed || busy,
        focusKey: "pairing-create-identity",
        onClick: actions.createPairingIdentity,
      }),
    ],
  });
}

function closeButton(ctx) {
  const { actions, computers, busy } = ctx;
  return computers.items.length
    ? button("Close", {
        variant: "ghost",
        size: "sm",
        disabled: busy,
        focusKey: "pairing-dismiss",
        onClick: actions.dismissPairing,
      })
    : null;
}

function chooseCard(ctx) {
  const { pairing, actions, pairingPending } = ctx;
  const disabled = !canStartPairing(pairing) || pairingPending;
  return card({
    title: "Add a computer",
    description: "Pick one on each computer: one shows a code, the other types it in.",
    children: [
      el("div", {
        className: "pair-choices",
        children: [
          choiceButton({
            iconName: "monitor",
            title: "Show a code",
            detail: "This computer shows a short code.",
            focusKey: "pairing-choose-show",
            disabled,
            onClick: actions.showPairingCode,
          }),
          choiceButton({
            iconName: "keyboard",
            title: "Enter a code",
            detail: "Type the code the other computer shows.",
            focusKey: "pairing-choose-enter",
            disabled,
            onClick: () => actions.choosePairingMode("enter"),
          }),
        ],
      }),
    ],
    actions: [closeButton(ctx)].filter(Boolean),
  });
}

function choiceButton({ iconName, title, detail, focusKey, disabled, onClick }) {
  const node = el("button", {
    className: "pair-choice",
    attrs: { type: "button" },
    dataset: { focusKey },
    children: [
      el("span", {
        className: "pair-choice-icon",
        attrs: { "aria-hidden": "true" },
        children: [icon(iconName)],
      }),
      el("span", {
        className: "pair-choice-copy",
        children: [el("strong", { text: title }), el("span", { text: detail })],
      }),
    ],
  });
  node.disabled = disabled;
  node.addEventListener("click", onClick);
  return node;
}

function enterCard(ctx) {
  const { pairing, actions, pairingPending } = ctx;
  const error = pairing.entryError;
  const input = el("input", {
    className: "input pair-code-input",
    attrs: {
      id: PAIRING_CODE_INPUT_ID,
      type: "text",
      inputmode: "text",
      autocomplete: "off",
      autocapitalize: "characters",
      spellcheck: "false",
      placeholder: CODE_PLACEHOLDER,
      "aria-label": "Code from the other computer",
      "aria-invalid": error ? "true" : null,
      "aria-describedby": error ? CODE_ERROR_ID : null,
    },
  });
  input.value = pairing.entry;
  input.disabled = pairingPending || !canStartPairing(pairing);
  input.addEventListener("input", (event) => {
    const caret = input.selectionStart ?? input.value.length;
    let formatted = formatCodeInput(input.value, caret);
    // Backspace over a dash removes the symbol in front of it rather than doing nothing.
    if (
      event.inputType === "deleteContentBackward" &&
      formatted.symbols === codeSymbols(pairing.entry) &&
      formatted.caret > 0
    ) {
      const symbols = formatted.symbols;
      const at = codeSymbols(input.value.slice(0, caret)).length;
      formatted = formatCodeInput(symbols.slice(0, at - 1) + symbols.slice(at), at - 1);
    }
    input.value = formatted.value;
    input.setSelectionRange(formatted.caret, formatted.caret);
    actions.editPairingCode(formatted.value);
  });
  input.addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    event.preventDefault();
    actions.submitPairingCode();
  });
  return card({
    title: "Enter the code from the other computer",
    description: "On the other computer, choose Show a code.",
    children: [
      el("div", {
        className: "field",
        children: [
          input,
          presence(
            "pairing-code-error",
            error
              ? el("p", {
                  className: "note",
                  text: error,
                  attrs: { id: CODE_ERROR_ID, role: "alert" },
                  dataset: { tone: "danger" },
                })
              : null,
          ),
        ],
      }),
    ],
    actions: [
      button("Back", {
        variant: "ghost",
        size: "sm",
        disabled: pairingPending,
        focusKey: "pairing-enter-back",
        onClick: () => actions.choosePairingMode("choose"),
      }),
      button("Connect", {
        iconName: "link",
        disabled: !canSubmitPairingCode(pairing) || pairingPending,
        busy: pairingPending,
        focusKey: "pairing-connect",
        onClick: actions.submitPairingCode,
      }),
    ],
  });
}

function showCard(ctx) {
  const { pairing, actions, pairingPending } = ctx;
  const view = pairing.view;
  return card({
    title: "Type this code on the other computer",
    description: "On the other computer, open MonHop and choose Enter a code.",
    children: [codeDisplay(ctx, view)],
    actions: [
      cancelPairingButton(ctx, "ghost"),
      button("New code", {
        variant: "outline",
        size: "sm",
        iconName: "refresh-cw",
        disabled: pairingPending || view.code === null,
        focusKey: "pairing-new-code",
        onClick: actions.showPairingCode,
      }),
    ],
  });
}

function codeDisplay(ctx, view) {
  const groups = codeGroups(view.code);
  if (!groups.length)
    return el("div", {
      className: "pair-code",
      dataset: { pending: "true" },
      children: [
        el("p", {
          className: "pair-code-groups",
          text: CODE_PLACEHOLDER,
          attrs: { "aria-hidden": "true" },
        }),
        el("p", { className: "pair-countdown-text", text: "Preparing a code…" }),
      ],
    });
  const groupNodes = [];
  groups.forEach((group, index) => {
    if (index) groupNodes.push(el("span", { className: "pair-code-dash", text: "-" }));
    groupNodes.push(el("span", { text: group }));
  });
  return el("div", {
    className: "pair-code",
    children: [
      el("p", { className: "visually-hidden", text: `Code: ${spokenCode(view.code)}` }),
      el("p", {
        className: "pair-code-groups",
        attrs: { "aria-hidden": "true" },
        children: groupNodes,
      }),
      countdown(ctx),
    ],
  });
}

// The bar drains on the compositor from where the window started, so a re-render mid-way carries
// on instead of restarting; with reduced motion it steps to the current fraction each render.
function countdown(ctx) {
  const progress = countdownProgress(ctx.pairing, ctx.now);
  const left = codeTimeLeft(ctx.pairing.view, ctx.now) ?? 0;
  const fill = el("span", { className: "pair-countdown-fill" });
  if (progress) {
    fill.style.setProperty("--countdown-left", String(progress.left));
    fill.style.setProperty("--countdown-total", `${Math.round(progress.total)}ms`);
    fill.style.setProperty("--countdown-delay", `${-Math.round(progress.elapsed)}ms`);
    fill.dataset.running = "true";
  }
  return el("div", {
    className: "pair-countdown",
    children: [
      el("span", {
        className: "pair-countdown-track",
        attrs: { "aria-hidden": "true" },
        children: [fill],
      }),
      el("p", {
        className: "pair-countdown-text",
        attrs: { id: COUNTDOWN_TEXT_ID },
        text: countdownText(left),
      }),
    ],
  });
}

export function countdownText(left) {
  return `Expires in ${formatCountdown(left)}`;
}

function progressCard(ctx) {
  const view = ctx.pairing.view;
  const [title, detail] = PROGRESS_COPY[view.phase] ?? ["Working", "Keep this window open."];
  return stateCard({
    id: "pairing-state",
    tone: "checking",
    title,
    detail: view.message || detail,
    actions: view.phase === "stopping" ? [] : [cancelPairingButton(ctx)],
  });
}

function pairedCard(ctx) {
  const { pairing, actions, busy } = ctx;
  const view = pairing.view;
  const title = `Paired with ${platformLabel(view.peerPlatform)}`;
  const nextActions = [
    button("Pair another computer", {
      variant: "outline",
      size: "sm",
      disabled: busy,
      focusKey: "pairing-another",
      onClick: actions.beginPairing,
    }),
    button("Done", {
      size: "sm",
      focusKey: "pairing-done",
      onClick: actions.dismissPairing,
    }),
  ];
  if (!view.badge)
    return stateCard({
      id: "pairing-state",
      tone: "connected",
      iconName: "check",
      title,
      detail: "MonHop keeps it connected while both computers are on this network.",
      actions: nextActions,
    });
  const badge = pairBadge(view.badge, "lg");
  if (ctx.badgeElapsed < BADGE_ENTER_MS) {
    badge.dataset.enter = "true";
    badge.style.setProperty("--motion-delay", `${-Math.round(ctx.badgeElapsed)}ms`);
  }
  const node = card({
    id: "pairing-result",
    children: [
      el("div", {
        className: "pair-result",
        children: [
          el("div", {
            className: "pair-result-copy",
            attrs: { role: "status" },
            children: [
              el("strong", { text: title }),
              el("p", { text: "The other computer shows the same picture." }),
            ],
          }),
          badge,
          el("p", {
            className: "pair-badge-caption",
            text: pairBadgeLabel(view.badge),
            attrs: { "aria-hidden": "true" },
          }),
        ],
      }),
    ],
    actions: nextActions,
  });
  node.classList.add("pair-result-card");
  return node;
}

function errorCard(ctx, { title, detail, retry }) {
  const { pairing, actions, pairingPending, gate, busy } = ctx;
  const retryButton = {
    "new-code": () =>
      button("New code", {
        size: "sm",
        iconName: "refresh-cw",
        disabled: pairingPending,
        focusKey: "pairing-new-code",
        onClick: actions.showPairingCode,
      }),
    "enter-again": () =>
      button("Enter a new code", {
        size: "sm",
        disabled: pairingPending,
        focusKey: "pairing-enter-again",
        onClick: () => actions.choosePairingMode("enter"),
      }),
    reopen: () =>
      button("Try again", {
        variant: "outline",
        size: "sm",
        disabled: !gate.allowed || busy,
        onClick: actions.openPairing,
      }),
  }[retry]();
  const node = stateCard({
    id: "pairing-state",
    tone: "error",
    iconName: "triangle-alert",
    title,
    detail,
    actions: [retry === "reopen" ? null : cancelPairingButton(ctx), retryButton].filter(Boolean),
  });
  if (pairing.view?.phase !== "error" || pairing.view.storageOutcome !== "unverified") return node;
  return el("div", {
    className: "stack",
    children: [
      node,
      note("The saved pairing could not be confirmed. Do not assume it was removed.", "danger"),
    ],
  });
}

function cancelPairingButton(ctx, variant = "outline") {
  const { actions, state, pairingOperation } = ctx;
  return button("Cancel", {
    variant,
    size: "sm",
    disabled: !state.nativeAvailable || pairingOperation === "pairing_cancel",
    focusKey: "pairing-cancel",
    onClick: actions.cancelPairing,
  });
}

function localNetworkHelp(ctx) {
  const { actions, busy } = ctx;
  return createAccordion(
    "local-network-help",
    "local-network-help",
    "macOS Local Network access",
    note(
      "MonHop asks for Local Network access when you show or enter a code. If macOS never asked, allow MonHop in its settings, then try again.",
    ),
    el("div", {
      className: "card-actions",
      attrs: { "data-align": "start" },
      children: [
        button("Open Local Network settings", {
          variant: "ghost",
          size: "sm",
          disabled: busy,
          onClick: () => actions.openSettings("local-network"),
        }),
      ],
    }),
  );
}
