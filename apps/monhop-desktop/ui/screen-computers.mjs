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
  pairingProgress,
  pairingRole,
  pairingStep,
  platformLabel,
  spokenCode,
} from "./pairing-model.mjs";
import { inspectorHeader } from "./screen-ready.mjs";
import { createAccordion, presence } from "./accordion.mjs";
import { displayName, findComputer } from "./computers-model.mjs";
import {
  button,
  el,
  icon,
  iconButton,
  motionEase,
  motionEnabled,
  motionMs,
  note,
  pairBadge,
  platformGlyph,
  sinceChanged,
  swap,
} from "./dom.mjs";
import { pairBadgeLabel } from "./pair-badge-model.mjs";

const CODE_PLACEHOLDER = "XXXX-XXXX-XXXX";
const CODE_EXPIRED = "This code expired. Show a new code.";
export const PAIRING_CODE_INPUT_ID = "pairing-code-input";
const CODE_ERROR_ID = "pairing-code-error";
export const COUNTDOWN_TEXT_ID = "pairing-countdown-text";
// A badge drawn again within this window is still the same entrance, stagger included.
const BADGE_ENTER_MS = 900;
const ROLES = [
  { key: "show", label: "Show a code", focusKey: "pairing-choose-show" },
  { key: "enter", label: "Enter a code", focusKey: "pairing-choose-enter" },
];

// The New computer inspector: every step of the code exchange, from choosing a role to the
// pairing picture both computers show. Once paired, it is headed by the computer it added.
export function pairingInspector(ctx) {
  const step = pairingStep(ctx.pairing, ctx.now);
  const parts = stepParts(step, ctx);
  const signature = step === "expired" ? "error" : step;
  const children = [
    step === "paired" ? pairedHeader(ctx) : exchangeHeader(ctx),
    presence("pairing-roles", parts.roles ? roleControl(ctx, step) : null),
    // The step glides between heights while it cross-fades; the progress list and the actions stay
    // put across the steps that share them.
    presence(
      "pairing-step-panel",
      swap(
        "pairing-step",
        el("div", { className: "pairing-stage", children: [parts.body] }),
        signature,
        {
          block: true,
        },
      ),
    ),
    presence("pairing-progress", parts.progress ? progressList(ctx) : null),
    presence(
      "pairing-actions",
      parts.actions?.length
        ? swap(
            "pairing-actions",
            el("div", { className: "pairing-actions pairing-footer", children: parts.actions }),
            `${signature}:${parts.actions.length}`,
            { block: true },
          )
        : null,
    ),
    presence(
      "pairing-message",
      ctx.pairing.message
        ? el("div", {
            className: "pairing-message",
            children: [note(ctx.pairing.message, "danger")],
          })
        : null,
    ),
  ];
  if (ctx.state.snapshot?.platform === "macos") children.push(localNetworkHelp(ctx));
  return el("div", { className: "inspector-view", dataset: { kind: "new", step }, children });
}

// A pairing this session finished, as a computer's inspector may show it until the user moves on.
export function pairedResult(ctx) {
  return pairedBody(ctx, { withActions: true });
}

function exchangeHeader(ctx) {
  const { actions, busy, gate, pairingPending, pairingOperation } = ctx;
  return inspectorHeader({
    glyph: el("span", { className: "add-glyph" }),
    tone: "new",
    title: "New computer",
    meta: "Pair a computer on this network",
    actions: [
      iconButton({
        id: "pairing-reload",
        label: "Reload pairing",
        size: "sm",
        variant: "ghost",
        disabled: !gate.allowed || busy,
        busy: pairingPending && pairingOperation === "pairing_open",
        onClick: actions.openPairing,
      }),
    ],
  });
}

// Paired, the header is the computer the exchange added, as its sidebar row now is.
function pairedHeader(ctx) {
  const view = ctx.pairing.view;
  const computer = findComputer(ctx.computers, view.peerFingerprint);
  const platform = computer?.platform ?? view.peerPlatform;
  return inspectorHeader({
    glyph: platformGlyph(platform === "windows" ? "windows" : "macos"),
    tone: "local",
    title: pairedName(ctx),
    meta: [platformLabel(platform), computer?.address ?? view.peerAddress]
      .filter(Boolean)
      .join(" · "),
  });
}

function pairedName(ctx) {
  const view = ctx.pairing.view;
  const computer = findComputer(ctx.computers, view.peerFingerprint);
  return computer ? displayName(computer) : platformLabel(view.peerPlatform);
}

// What each step draws: its body, whether the role switch and the progress list show, and the
// actions under them.
function stepParts(step, ctx) {
  switch (step) {
    case "closed":
      return closedParts(ctx);
    case "identity":
      return identityParts(ctx);
    case "choose":
      return chooseParts(ctx);
    case "enter":
      return enterParts(ctx);
    case "show":
      return showParts(ctx);
    case "expired":
      // Drawn as the error the backend reports on its next status, so that status changes nothing.
      return errorParts(ctx, pairingErrorPresentation({ role: "showing", message: CODE_EXPIRED }));
    case "progress":
      return progressParts(ctx);
    case "paired":
      return { body: pairedBody(ctx, { withActions: true }) };
    default:
      return errorParts(ctx, pairingErrorPresentation(ctx.pairing.view));
  }
}

// ---------- the role switch ----------

// Show a code or Enter a code. Neither is picked until the user picks one; the thumb then glides
// between them and carries a glide on across renders.
function roleControl(ctx, step) {
  const { pairing, pairingPending, actions } = ctx;
  const role = pairingRole(pairing, step);
  const index = ROLES.findIndex((item) => item.key === role);
  const locked = step === "progress" || pairingPending;
  const node = el("div", {
    className: "segmented role-switch",
    attrs: { role: "radiogroup", "aria-label": "How to pair" },
    dataset: { picked: String(index >= 0) },
  });
  const thumb = el("span", { className: "role-switch-thumb", attrs: { "aria-hidden": "true" } });
  thumb.dataset.at = String(Math.max(index, 0));
  node.append(thumb);
  ROLES.forEach((item, at) => {
    const option = el("button", {
      className: "segmented-option",
      text: item.label,
      attrs: { type: "button", role: "radio", "aria-checked": String(at === index) },
      dataset: { focusKey: item.focusKey },
    });
    option.disabled = locked || (at !== index && !canSwitchRole(ctx, step));
    option.addEventListener("click", () => {
      if (at !== index || step === "expired") actions.switchPairingRole(item.key);
    });
    node.append(option);
  });
  // Arrow keys move between the two ways in, as in any radio group.
  node.addEventListener("keydown", (event) => {
    if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(event.key)) return;
    const options = [...node.querySelectorAll(".segmented-option")];
    const next = options[options.indexOf(document.activeElement) === 0 ? 1 : 0];
    if (!next || next.disabled) return;
    event.preventDefault();
    next.focus();
    next.click();
  });
  glideThumb(thumb, index);
  return el("section", { className: "inspector-section pairing-roles", children: [node] });
}

function canSwitchRole(ctx, step) {
  return canStartPairing(ctx.pairing) || ["show", "expired"].includes(step);
}

// The role last picked, where the thumb set off from and when. A re-render mid-glide replays the
// same glide at the same point; a new pick sets off from wherever the thumb is drawn right now.
let thumbGlide = null;

function glideThumb(thumb, index) {
  const now = performance.now();
  if (thumbGlide?.to !== index) {
    const shown = [...document.querySelectorAll(".role-switch-thumb")].find(
      (node) => !node.closest("[data-exit]"),
    );
    const style = thumbGlide && shown && motionEnabled() ? getComputedStyle(shown) : null;
    thumbGlide = {
      to: index,
      from: style ? { transform: style.transform, opacity: style.opacity } : null,
      at: now,
    };
  }
  const { from, at } = thumbGlide;
  const duration = motionMs("--motion-spring");
  const elapsed = now - at;
  if (!from || index < 0 || !motionEnabled() || elapsed >= duration) return;
  // Its resting place is known once it is in the page, which happens before the next paint.
  queueMicrotask(() => {
    if (!thumb.isConnected) return;
    const rest = getComputedStyle(thumb).transform;
    const animation = thumb.animate(
      [
        { transform: from.transform, opacity: from.opacity },
        { transform: rest, opacity: 1 },
      ],
      { duration, easing: motionEase("--ease-spring") },
    );
    animation.currentTime = elapsed;
  });
}

// ---------- the steps ----------

function closedParts(ctx) {
  const { state, actions, busy, gate, pairingPending } = ctx;
  if (!gate.allowed)
    return {
      body: notice({ tone: "muted", title: "Finish Get ready first", detail: gate.detail }),
    };
  if (pairingPending)
    return {
      body: notice({
        tone: "checking",
        title: "Opening pairing",
        detail:
          state.snapshot?.platform === "macos"
            ? "Reading the saved identity. Approve the Keychain prompt if macOS shows one."
            : "Reading the saved identity.",
      }),
    };
  return {
    body: notice({
      tone: "neutral",
      iconName: "link",
      title: "Add a computer",
      detail: gate.detail,
    }),
    actions: [button("Open pairing", { size: "sm", disabled: busy, onClick: actions.openPairing })],
  };
}

function identityParts(ctx) {
  const { pairing, actions, busy, gate } = ctx;
  return {
    body: notice({
      tone: "neutral",
      iconName: "key",
      title: "Create this computer's identity",
      detail:
        pairing.view.message ||
        "MonHop creates a private key once on this computer. It never leaves this computer.",
    }),
    actions: [
      button("Create identity", {
        size: "sm",
        disabled: !gate.allowed || busy,
        focusKey: "pairing-create-identity",
        onClick: actions.createPairingIdentity,
      }),
    ],
  };
}

function chooseParts(ctx) {
  return {
    roles: true,
    body: el("div", {
      className: "pairing-body",
      children: [
        el("p", {
          className: "pairing-lead",
          text: "Pick one on each computer: one shows a code, the other types it in.",
        }),
      ],
    }),
    actions: [closeButton(ctx)].filter(Boolean),
  };
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

function showParts(ctx) {
  const { pairing, actions, pairingPending } = ctx;
  const view = pairing.view;
  return {
    roles: true,
    progress: true,
    body: el("div", {
      className: "pairing-body",
      children: [
        codeDisplay(ctx, view),
        el("p", {
          className: "pairing-lead",
          text: "On the other computer, open MonHop and choose Enter a code.",
        }),
        el("p", {
          className: "pairing-waiting",
          attrs: { role: "status" },
          children: [
            el("span", { className: "spinner", attrs: { "aria-hidden": "true" } }),
            el("span", { text: "Waiting for the other computer" }),
          ],
        }),
      ],
    }),
    actions: [
      cancelPairingButton(ctx),
      button("New code", {
        variant: "secondary",
        size: "sm",
        iconName: "refresh-cw",
        disabled: pairingPending || view.code === null,
        focusKey: "pairing-new-code",
        onClick: actions.showPairingCode,
      }),
    ],
  };
}

function codeDisplay(ctx, view) {
  const groups = codeGroups(view.code);
  if (!groups.length)
    return el("div", {
      className: "pairing-code",
      dataset: { pending: "true" },
      children: [
        el("p", {
          className: "pairing-code-groups",
          text: CODE_PLACEHOLDER,
          attrs: { "aria-hidden": "true" },
        }),
        el("p", { className: "pairing-expiry-text", text: "Preparing a code…" }),
      ],
    });
  const groupNodes = [];
  groups.forEach((group, index) => {
    if (index) groupNodes.push(el("span", { className: "pairing-code-dash", text: "-" }));
    groupNodes.push(el("span", { text: group }));
  });
  return el("div", {
    className: "pairing-code-block",
    children: [
      el("div", {
        className: "pairing-code",
        children: [
          el("p", { className: "visually-hidden", text: `Code: ${spokenCode(view.code)}` }),
          el("p", {
            className: "pairing-code-groups",
            attrs: { "aria-hidden": "true" },
            children: groupNodes,
          }),
        ],
      }),
      countdown(ctx),
    ],
  });
}

// The hairline drains on the compositor from where the window started, so a re-render mid-way
// carries on instead of restarting; with reduced motion it steps to the current fraction each render.
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
    className: "pairing-expiry",
    children: [
      el("span", {
        className: "pairing-expiry-track",
        attrs: { "aria-hidden": "true" },
        children: [fill],
      }),
      el("p", {
        className: "pairing-expiry-text",
        attrs: { id: COUNTDOWN_TEXT_ID },
        text: countdownText(left),
      }),
    ],
  });
}

export function countdownText(left) {
  return `Expires in ${formatCountdown(left)}`;
}

function enterParts(ctx) {
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
  return {
    roles: true,
    body: el("div", {
      className: "pairing-body",
      children: [
        el("div", {
          className: "field",
          children: [
            input,
            presence(
              "pairing-code-error",
              error
                ? el("p", {
                    className: "note pairing-field-error",
                    attrs: { id: CODE_ERROR_ID, role: "alert" },
                    dataset: { tone: "danger" },
                    children: [icon("triangle-alert", 12), el("span", { text: error })],
                  })
                : null,
            ),
          ],
        }),
        el("p", { className: "pairing-lead", text: "On the other computer, choose Show a code." }),
      ],
    }),
    actions: [
      button("Cancel", {
        variant: "ghost",
        size: "sm",
        disabled: pairingPending,
        focusKey: "pairing-enter-back",
        onClick: () => actions.choosePairingMode("choose"),
      }),
      button("Connect", {
        size: "sm",
        iconName: "link",
        disabled: !canSubmitPairingCode(pairing) || pairingPending,
        busy: pairingPending,
        focusKey: "pairing-connect",
        onClick: actions.submitPairingCode,
      }),
    ],
  };
}

function progressParts(ctx) {
  const view = ctx.pairing.view;
  return {
    roles: true,
    progress: true,
    body:
      view.phase === "stopping"
        ? notice({ tone: "checking", title: "Stopping", detail: view.message || "Cleaning up." })
        : el("div", {
            className: "pairing-body",
            children: [
              el("p", {
                className: "pairing-lead",
                text: "Keep MonHop open on both computers until this finishes.",
              }),
            ],
          }),
    actions: view.phase === "stopping" ? [] : [cancelPairingButton(ctx)],
  };
}

// Connecting, Checking the code, Saving the pairing: the current step turns, the ones behind it are
// checked, and a check pops in once as its step finishes.
function progressList(ctx) {
  const view = ctx.pairing.view;
  const steps = pairingProgress(view);
  return el("section", {
    className: "inspector-section pairing-progress",
    attrs: { "aria-label": "Progress" },
    children: [
      el("div", {
        className: "inspector-section-heading",
        children: [el("h3", { text: "Progress" })],
      }),
      el("ol", {
        className: "pairing-steps",
        children: steps.map((step) => {
          const elapsed = sinceChanged(`pairing-progress-${step.key}`, step.state);
          const mark = el("span", {
            className: "pairing-step-mark",
            attrs: { "aria-hidden": "true" },
            children: [
              step.state === "live" ? el("span", { className: "spinner" }) : icon("check", 12),
            ],
          });
          if (elapsed < motionMs("--motion-spring")) {
            mark.dataset.enter = "true";
            mark.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
          }
          return el("li", {
            className: "pairing-step",
            dataset: { state: step.state },
            attrs: { "aria-current": step.state === "live" ? "step" : null },
            children: [
              mark,
              el("div", {
                className: "pairing-step-copy",
                children: [
                  el("strong", { text: step.title }),
                  el("span", {
                    text: step.state === "live" && view.message ? view.message : step.detail,
                  }),
                ],
              }),
            ],
          });
        }),
      }),
    ],
  });
}

function pairedBody(ctx, { withActions }) {
  const { pairing, actions, busy } = ctx;
  const view = pairing.view;
  const name = pairedName(ctx);
  const nextActions = withActions
    ? el("div", {
        className: "pairing-actions",
        children: [
          button("Pair another computer", {
            variant: "secondary",
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
        ],
      })
    : null;
  const title = el("p", {
    className: "pairing-result-title",
    attrs: { role: "status" },
    children: [icon("check", 16), el("strong", { text: `Paired with ${name}` })],
  });
  if (!view.badge)
    return el("section", {
      className: "inspector-section pairing-result",
      attrs: { id: "pairing-result" },
      children: [
        title,
        el("p", {
          className: "pairing-lead",
          text: "MonHop keeps it connected while both computers are on this network.",
        }),
        nextActions,
      ],
    });
  const badgeElapsed = sinceChanged("pairing-badge", view.peerFingerprint);
  const picture = pairBadge(view.badge, "lg");
  if (badgeElapsed < BADGE_ENTER_MS) {
    picture.dataset.enter = "true";
    picture.style.setProperty("--motion-delay", `${-Math.round(badgeElapsed)}ms`);
  }
  return el("section", {
    className: "inspector-section pairing-result",
    attrs: { id: "pairing-result" },
    children: [
      title,
      picture,
      el("p", {
        className: "pairing-result-caption",
        children: [
          el("strong", { text: `${pairBadgeLabel(view.badge)}.` }),
          el("span", { text: ` ${name} shows the same picture.` }),
        ],
      }),
      el("p", {
        className: "pairing-lead",
        text: "If they differ, forget this computer and pair again.",
      }),
      nextActions,
    ],
  });
}

function errorParts(ctx, { title, detail, retry, settings = null }) {
  const { pairing, actions, pairingPending, gate, busy } = ctx;
  const settingsButton =
    settings === "local-network"
      ? button("Open Local Network settings", {
          variant: "secondary",
          size: "sm",
          disabled: busy,
          focusKey: "pairing-local-network",
          onClick: () => actions.openSettings("local-network"),
        })
      : null;
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
        variant: "secondary",
        size: "sm",
        disabled: !gate.allowed || busy,
        focusKey: "pairing-reopen",
        onClick: actions.openPairing,
      }),
  }[retry]();
  const unverified =
    pairing.view?.phase === "error" && pairing.view.storageOutcome === "unverified";
  return {
    roles: retry !== "reopen",
    body: el("div", {
      className: "pairing-body",
      children: [
        notice({ tone: "error", iconName: "triangle-alert", title, detail }),
        settingsButton
          ? el("div", {
              className: "pairing-actions",
              dataset: { align: "start" },
              children: [settingsButton],
            })
          : null,
        unverified
          ? note(
              "The saved pairing could not be confirmed. Do not assume it was removed.",
              "danger",
            )
          : null,
      ],
    }),
    actions: [retry === "reopen" ? null : cancelPairingButton(ctx), retryButton].filter(Boolean),
  };
}

// A status line with its glyph: a spinner while something runs, else the tone's icon.
function notice({ tone, iconName = "link", title, detail }) {
  return el("div", {
    className: "pairing-notice",
    dataset: { tone },
    attrs: { role: tone === "error" ? "alert" : "status" },
    children: [
      el("span", {
        className: "pairing-notice-glyph",
        attrs: { "aria-hidden": "true" },
        children: [tone === "checking" ? el("span", { className: "spinner" }) : icon(iconName, 16)],
      }),
      el("div", {
        className: "pairing-notice-copy",
        children: [el("strong", { text: title }), detail ? el("p", { text: detail }) : null],
      }),
    ],
  });
}

function cancelPairingButton(ctx) {
  const { actions, state, pairingOperation } = ctx;
  return button("Cancel", {
    variant: "ghost",
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
