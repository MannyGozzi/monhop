import { displayName } from "./computers-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import { computerCard } from "./computer-card.mjs";
import { createDashboardArrangement } from "./dashboard-arrangement.mjs";
import { displayNoticeCopy } from "./sharing-model.mjs";
import {
  DIM_LEVEL_STEP,
  dimButtonLabel,
  dimmingMessage,
  levelLabel,
  shortcutDescription,
} from "./dimming-model.mjs";
import { canInstall, installHint } from "./updates-model.mjs";
import {
  button,
  card,
  clear,
  copyFeedbackControls,
  el,
  note,
  presence,
  sliderRow,
  swap,
  switchRow,
} from "./dom.mjs";

export function renderHome(nodes, ctx) {
  const { computers, state, actions, activeComputer, computersLoadFailure } = ctx;
  clear(nodes.homeContent);

  // The ready card is this computer's own update, not tied to a paired computer or connection.
  const ready = presence("home-updates-ready", updatesReadyCard(ctx));
  if (ready) nodes.homeContent.append(ready);

  if (computersLoadFailure) {
    nodes.homeContent.append(
      card({
        title: "Paired computers unavailable",
        description:
          "Check again to read the list. This changes nothing about pairing or the connection.",
        tone: "muted",
      }),
    );
  } else if (computers.items.length === 0) {
    nodes.homeContent.append(
      card({
        id: "home-empty",
        title: "Set up a computer",
        description:
          "MonHop shares one keyboard and mouse with another computer on your own network. You pair each computer once; after that MonHop keeps them connected.",
        actions: [
          button("Set up a computer", {
            iconName: "link",
            focusKey: "home-setup",
            onClick: () => actions.goToPage("setup"),
          }),
        ],
      }),
    );
  } else {
    const notice = presence("home-display-notice", displayNoticeCard(ctx));
    if (notice) nodes.homeContent.append(notice);
    if (activeComputer)
      nodes.homeContent.append(
        computerCard(ctx, activeComputer, {
          scope: "home",
          // This card draws the full-size arrangement itself, with the button that changes it.
          viewport: false,
          extras: activeExtras(ctx),
          details: activeDetails(ctx),
        }),
      );
    for (const computer of computers.items)
      if (computer !== activeComputer)
        nodes.homeContent.append(computerCard(ctx, computer, { scope: "home" }));
  }

  nodes.homeContent.append(dimmingCard(ctx));

  const logPath = state.snapshot?.logPath;
  if (typeof logPath === "string" && logPath)
    nodes.homeContent.append(
      el("div", {
        className: "progress-line",
        children: [
          el("span", { text: `Log file: ${logPath}` }),
          button("Show", { size: "sm", variant: "outline", onClick: actions.revealLogFile }),
        ],
      }),
    );
}

// Shown whenever a downloaded build is ready, whether or not a computer is paired or in use.
function updatesReadyCard({ updates, actions }) {
  const { view, pending } = updates;
  if (view.phase !== "ready") return null;
  const hint = installHint(view);
  return card({
    id: "home-updates-ready",
    tone: "success",
    title: `MonHop ${view.availableVersion} is ready`,
    description: "It installs when you quit MonHop, or now.",
    children: hint ? [note(hint, "danger")] : [],
    actions: [
      button("Restart to update", {
        id: "home-updates-install",
        disabled: pending || !canInstall(view),
        busy: pending,
        onClick: actions.installUpdate,
      }),
    ],
  });
}

// Only the active computer's own displays can be rearranged, so the banner needs one to point at.
// A notice MonHop is already settling belongs on that computer's card, not across the page.
function displayNoticeCard(ctx) {
  const { actions, busy, peerName, sharing, activeComputer } = ctx;
  const notice = sharing.view?.displayNotice;
  if (!notice || !activeComputer) return null;
  const copy = displayNoticeCopy(notice.kind, peerName);
  if (!copy || copy.presentation !== "banner") return null;
  return card({
    id: "home-display-notice",
    tone: "warning",
    title: copy.title,
    description: copy.body,
    actions: [
      button(copy.primaryLabel, {
        disabled: busy,
        focusKey: "home-display-notice-arrange",
        onClick: actions.changeLayout,
      }),
      button(copy.secondaryLabel, {
        variant: "outline",
        disabled: busy,
        focusKey: "home-display-notice-dismiss",
        onClick: actions.dismissDisplayNotice,
      }),
    ],
  });
}

// The card works without a paired computer: dimming is this computer's own feature.
function dimmingCard({ core, dimming, actions }) {
  const view = dimming.view;
  const disabled = !core || dimming.pending;
  const message = dimmingMessage(dimming);
  return card({
    id: "home-dimming",
    title: "Screen dimming",
    description:
      "Darkens every display of this computer, above everything, until you toggle it again.",
    children: [
      switchRow("Shortcut", {
        checked: view?.enabled ?? true,
        description: shortcutDescription(view),
        disabled,
        focusKey: "home-dimming-shortcut",
        onChange: actions.setDimmingEnabled,
      }),
      sliderRow("Darkness", {
        value: view?.level ?? 50,
        min: view?.minLevel ?? 10,
        max: view?.maxLevel ?? 99,
        step: DIM_LEVEL_STEP,
        format: levelLabel,
        description: "Changes live while the screen is dimmed.",
        disabled,
        focusKey: "home-dimming-level",
        onInput: actions.previewDimLevel,
        onChange: actions.setDimLevel,
        onDragStart: actions.beginDimDrag,
        onDragEnd: actions.endDimDrag,
      }),
      presence("home-dimming-message", message ? note(message, "danger") : null),
    ],
    actions: [
      button(dimButtonLabel(view), {
        variant: view?.dimmed ? "outline" : "default",
        iconName: view?.dimmed ? "sun" : "moon",
        disabled,
        busy: dimming.pending,
        focusKey: "home-dimming-toggle",
        onClick: actions.toggleDimming,
      }),
    ],
  });
}

// The computer in use carries what the others cannot: its saved layout.
function activeExtras(ctx) {
  const { actions, busy, platform, state, activeComputer } = ctx;
  const saved = activeComputer.setup?.saved === true;
  const localPlatform = state.snapshot?.platform ?? platform;
  const key = `home-${activeComputer.fingerprint}`;
  return [
    el("div", {
      className: "home-layout",
      children: [
        swap(
          `${key}-layout`,
          saved
            ? createDashboardArrangement(activeComputer.setup, {
                local: platformLabel(localPlatform, true),
                peer: displayName(activeComputer),
                localPlatform,
                peerPlatform: activeComputer.platform,
                motionKey: `${key}-layout`,
              })
            : note("No layout yet. Arrange the displays once to start sharing input."),
          saved ? "layout" : "none",
          { block: true },
        ),
        el("div", {
          className: "card-actions",
          attrs: { "data-align": "start" },
          children: [
            button(saved ? "Change layout" : "Arrange displays", {
              variant: "outline",
              size: "sm",
              disabled: busy,
              focusKey: "home-change-layout",
              onClick: actions.changeLayout,
            }),
          ],
        }),
      ],
    }),
  ].filter(Boolean);
}

// The last drop stays out of Home; it only shows inside Details, never in red, and always with a Copy button.
function activeDetails(ctx) {
  const { actions, busy, sharing, dropCopyFeedback, activeComputer } = ctx;
  const lastFailure = sharing.view?.lastFailure;
  const feedback = dropCopyFeedback;
  const key = `home-${activeComputer.fingerprint}`;
  const copy = copyFeedbackControls(feedback, {
    disabled: busy || feedback.state === "pending",
    focusKey: "home-copy-last-drop",
    onClick: actions.copyLastDrop,
  });
  return [
    presence(
      `${key}-last-drop`,
      lastFailure
        ? el("div", {
            className: "field",
            children: [
              note(lastFailure),
              el("div", {
                className: "card-actions",
                attrs: { "data-align": "start" },
                children: [copy.button],
              }),
              copy.line,
            ],
          })
        : null,
    ),
  ];
}
