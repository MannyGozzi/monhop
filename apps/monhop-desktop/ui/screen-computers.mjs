import {
  canConfirmPairing,
  canInspectPairing,
  formatFingerprint,
  localNetworkStatus,
  platformLabel,
} from "./pairing-model.mjs";
import { computerCard } from "./computer-card.mjs";
import { createAccordion } from "./accordion.mjs";
import {
  button,
  card,
  clear,
  copyFeedbackControls,
  el,
  fingerprintBlock,
  iconButton,
  note,
  stateCard,
  textarea,
} from "./dom.mjs";

const BUSY_COPY = {
  "requesting-network": [
    "Requesting network access",
    "Approve the macOS prompt if one appears. Keep this window open.",
  ],
  waiting: [
    "Waiting for the other computer",
    "Press Pair on the other computer too. This can take up to two minutes.",
  ],
  connecting: ["Exchanging keys", "Keep MonHop open on both computers."],
  saving: ["Saving the pairing", "Almost done."],
  stopping: ["Stopping", "Cleaning up."],
};

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
    if (computers.items.length === 0)
      nodes.computersContent.append(
        card({
          title: "Pair a computer",
          description: "Add another computer on this network. You compare fingerprints once.",
        }),
      );
    return;
  }
  if (!showPairing) {
    nodes.computersContent.append(
      card({
        title: "Pair a computer",
        description: "Add another computer on this network. You compare fingerprints once.",
        actions: [
          button("Pair a computer", {
            iconName: "link",
            disabled: busy,
            focusKey: "pairing-begin",
            onClick: actions.beginPairing,
          }),
        ],
      }),
    );
    return;
  }
  renderExchange(nodes, ctx);
}

function renderExchange(nodes, ctx) {
  const { state, pairing, actions, busy, gate, pairingPending, pairingOperation } = ctx;
  const view = pairing.view;
  if (
    pairingPending &&
    ["pairing_confirm", "pairing_request_network_access", "pairing_cancel"].includes(
      pairingOperation,
    )
  ) {
    nodes.computersContent.append(
      busyCard(ctx, {
        ...view,
        phase: pairingOperation === "pairing_confirm" ? "connecting" : "requesting-network",
      }),
    );
    return;
  }
  if (!view || view.phase === "closed") {
    if (!gate.allowed)
      nodes.computersContent.append(
        card({ title: "Finish Get ready first", description: gate.detail, tone: "muted" }),
      );
    else if (pairingPending)
      nodes.computersContent.append(
        stateCard({
          id: "pairing-state",
          tone: "checking",
          title: "Opening pairing",
          detail:
            state.snapshot?.platform === "macos"
              ? "Reading the saved identity. Approve the Keychain prompt if macOS shows one."
              : "Reading the saved identity.",
        }),
      );
    else
      nodes.computersContent.append(
        card({
          title: "Pair a computer",
          description: gate.detail,
          actions: [button("Open pairing", { disabled: busy, onClick: actions.openPairing })],
        }),
      );
    if (pairing.message) nodes.computersContent.append(note(pairing.message, "danger"));
    return;
  }
  switch (view.phase) {
    case "identity-missing":
      renderIdentityMissing(nodes, ctx);
      break;
    case "ready":
      renderCodeExchange(nodes, ctx);
      break;
    case "review":
      if (pairing.candidateStale) renderCodeExchange(nodes, ctx);
      else renderReview(nodes, ctx);
      break;
    case "requesting-network":
    case "waiting":
    case "connecting":
    case "saving":
    case "stopping":
      nodes.computersContent.append(busyCard(ctx, view));
      break;
    case "paired":
      renderPaired(nodes, ctx);
      break;
    default:
      renderPairingError(nodes, ctx);
      break;
  }
  if (pairing.message) nodes.computersContent.append(note(pairing.message, "danger"));
  if (state.snapshot?.platform === "macos" && view.phase !== "paired")
    nodes.computersContent.append(localNetworkHelp(ctx));
}

function renderIdentityMissing(nodes, ctx) {
  const { pairing, actions, busy, gate } = ctx;
  nodes.computersContent.append(
    card({
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
    }),
  );
}

function renderCodeExchange(nodes, ctx) {
  const { pairing, actions, busy, computers } = ctx;
  const view = pairing.view;
  nodes.computersContent.append(
    card({
      title: "1. Give this code to the other computer",
      description:
        "Paste it into the same field on the other computer. It carries an address and a public certificate, never a private key.",
      children: [codeBlock(ctx, view)],
    }),
    card({
      title: "2. Paste the other computer's code here",
      children: [
        textarea({
          id: "peer-connection-code",
          value: pairing.candidateCode,
          placeholder: "LKM2:…",
          rows: 3,
          ariaLabel: "The other computer's code",
          maxLength: 6200,
          disabled: busy,
          onInput: (node) => {
            const start = node.selectionStart;
            const end = node.selectionEnd;
            actions.editCandidate(node.value);
            const updated = document.querySelector("#peer-connection-code");
            if (updated) {
              updated.focus();
              updated.setSelectionRange(start, end);
            }
          },
        }),
      ],
      actions: [
        computers.items.length
          ? button("Close", {
              variant: "ghost",
              size: "sm",
              disabled: busy,
              focusKey: "pairing-dismiss",
              onClick: actions.dismissPairing,
            })
          : null,
        button("Check code", {
          disabled: !canInspectPairing(pairing) || busy,
          focusKey: "pairing-inspect",
          onClick: actions.inspectPairingCode,
        }),
      ].filter(Boolean),
    }),
  );
}

function renderReview(nodes, ctx) {
  const { pairing, actions, busy } = ctx;
  const view = pairing.view;
  const peerLabel = view.peerPlatform ? platformLabel(view.peerPlatform) : "Other computer";
  const children = [
    el("div", {
      className: "fingerprint-pair",
      children: [
        fingerprintBlock("This computer", formatFingerprint(view.localFingerprint)),
        fingerprintBlock(peerLabel, formatFingerprint(view.peerFingerprint)),
      ],
    }),
  ];
  if (view.peerAddress) children.push(note(`Other computer: ${view.peerAddress}`));
  nodes.computersContent.append(
    card({
      title: "Compare fingerprints on both computers",
      description:
        "Read the two fingerprints aloud or side by side. Both computers must show the same pair. Then press Pair on each.",
      children,
      actions: [
        button("Change code", {
          variant: "ghost",
          size: "sm",
          disabled: busy,
          focusKey: "pairing-change-code",
          onClick: () => actions.editCandidate(pairing.candidateCode),
        }),
        button(pairing.compared ? "Fingerprints match" : "They match", {
          variant: "outline",
          pressed: pairing.compared,
          disabled: busy,
          focusKey: "pairing-fingerprint-compared",
          iconName: pairing.compared ? "check" : null,
          onClick: actions.toggleCompared,
        }),
        button("Pair", {
          disabled: !canConfirmPairing(pairing) || busy,
          focusKey: "pairing-confirm",
          iconName: "link",
          onClick: actions.confirmPairing,
        }),
      ],
    }),
    createAccordion(
      "pairing-your-code",
      "pairing-your-code",
      "Your code, in case the other computer still needs it",
      codeBlock(ctx, view),
    ),
  );
}

function renderPaired(nodes, ctx) {
  const { pairing, actions, busy, peerName } = ctx;
  nodes.computersContent.append(
    stateCard({
      id: "pairing-state",
      tone: "connected",
      iconName: "check",
      title: "Paired",
      detail: `${peerName} is set up. MonHop keeps it connected while both computers are on this network.`,
      actions: [
        button("Pair another computer", {
          variant: "outline",
          disabled: !pairing.view || busy,
          onClick: actions.openPairing,
        }),
      ],
    }),
  );
}

function renderPairingError(nodes, ctx) {
  const { pairing, actions, busy, gate } = ctx;
  nodes.computersContent.append(
    stateCard({
      id: "pairing-state",
      tone: "error",
      iconName: "triangle-alert",
      title: "Pairing needs attention",
      detail: pairing.view.message || "The pairing did not finish.",
      actions: [
        button("Try again", {
          variant: "outline",
          disabled: !gate.allowed || busy,
          onClick: actions.openPairing,
        }),
      ],
    }),
  );
  if (pairing.view.storageOutcome === "unverified")
    nodes.computersContent.append(
      note("The saved pairing could not be confirmed. Do not assume it was removed.", "danger"),
    );
}

function busyCard(ctx, view) {
  const copy = BUSY_COPY[view?.phase] ?? ["Working", "Keep this window open."];
  return stateCard({
    id: "pairing-state",
    tone: "checking",
    title: copy[0],
    detail: view?.message || copy[1],
    actions: [cancelPairingButton(ctx)],
  });
}

function cancelPairingButton(ctx) {
  const { actions, state, pairingOperation } = ctx;
  return button("Cancel", {
    variant: "outline",
    size: "sm",
    disabled: !state.nativeAvailable || pairingOperation === "pairing_cancel",
    focusKey: "pairing-cancel",
    onClick: actions.cancelPairing,
  });
}

function localNetworkHelp(ctx) {
  const { pairing, actions, busy } = ctx;
  const status = localNetworkStatus(pairing);
  return createAccordion(
    "local-network-help",
    "local-network-help",
    "macOS Local Network access",
    note(`Local Network access: ${status.label}. ${status.detail}`),
    note(
      "If macOS never showed a Local Network prompt for MonHop, request it here after comparing codes, then allow it.",
    ),
    el("div", {
      className: "card-actions",
      attrs: { "data-align": "start" },
      children: [
        pairing.view?.phase === "review"
          ? button("Request network access", {
              variant: "outline",
              size: "sm",
              disabled: !canConfirmPairing(pairing) || busy,
              focusKey: "pairing-request-network-access",
              onClick: actions.requestNetworkAccess,
            })
          : null,
        button("Open Local Network settings", {
          variant: "ghost",
          size: "sm",
          disabled: busy,
          onClick: () => actions.openSettings("local-network"),
        }),
      ].filter(Boolean),
    }),
  );
}

function codeBlock(ctx, view) {
  const { actions, busy, copyFeedback } = ctx;
  const pending = copyFeedback.state === "pending";
  const copy = copyFeedbackControls(copyFeedback, {
    disabled: !view.localCode || busy || pending,
    focusKey: "pairing-copy-code",
    onClick: actions.copyPairingCode,
  });
  return el("div", {
    className: "field",
    children: [
      el("div", {
        className: "code-block",
        children: [
          textarea({
            id: "local-connection-code",
            value: view.localCode || "Not available",
            readOnly: true,
            rows: 3,
            ariaLabel: "This computer's code",
          }),
          copy.button,
        ],
      }),
      copy.line,
    ],
  });
}
