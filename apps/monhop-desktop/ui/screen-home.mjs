import { displayName } from "./computers-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import { presence } from "./accordion.mjs";
import { computerCard } from "./computer-card.mjs";
import { createDashboardArrangement, DRAWN_DISPLAYS_NOTE } from "./dashboard-arrangement.mjs";
import { homeEntries } from "./home-model.mjs";
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
  rows,
  sliderRow,
  swap,
  switchRow,
} from "./dom.mjs";

export function renderHome(nodes, ctx) {
  const { computers, state, actions, active, computersLoadFailure } = ctx;
  clear(nodes.homeContent);

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
    // With zero or one computer switched in, `homeEntries` hands back the same lone hero entry
    // this screen has always drawn (or none); with several, the group picture leads and each gets
    // its own live card instead.
    const entries = homeEntries(computers, ctx.sharing.view, active);
    for (const entry of entries) {
      if (entry.type === "group") nodes.homeContent.append(groupArrangementCard(ctx, entries));
      else if (entry.type === "hero" && ctx.activeComputer)
        // `ctx.activeComputer`, not `entry.computer`: today's single-computer path keys everything
        // (its saved layout, its last drop) off that one field, so the hero card keeps doing the same.
        nodes.homeContent.append(
          computerCard(ctx, ctx.activeComputer, {
            scope: "home",
            // This card draws the full-size arrangement itself, with the button that changes it.
            viewport: false,
            extras: activeExtras(ctx),
            details: activeDetails(ctx),
          }),
        );
      else if (entry.type === "live")
        nodes.homeContent.append(computerCard(ctx, entry.computer, { scope: "home" }));
    }
    const notice = presence("home-display-notice", displayNoticeCard(ctx));
    if (notice) nodes.homeContent.append(notice);
    for (const entry of entries)
      if (entry.type === "other")
        nodes.homeContent.append(computerCard(ctx, entry.computer, { scope: "home" }));
  }

  // The hero stays first. Updates and machine preferences are secondary to the computer in use.
  const ready = presence("home-updates-ready", updatesReadyCard(ctx));
  if (ready) nodes.homeContent.append(ready);
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
      rows([
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
      ]),
      presence("home-dimming-message", message ? note(message, "danger") : null),
    ],
    actions: [
      button(dimButtonLabel(view), {
        variant: "secondary",
        iconName: view?.dimmed ? "sun" : "moon",
        disabled,
        busy: dimming.pending,
        focusKey: "home-dimming-toggle",
        onClick: actions.toggleDimming,
      }),
    ],
  });
}

// Once more than one computer is switched in, the group's own picture leads instead of any one
// computer's card carrying it. The app layer does not yet hand this screen a whole-group saved
// layout to draw (each paired computer's own `setup` only ever covers this computer and that one
// other), so this names who is sharing and links to the editor rather than guessing at a picture it
// cannot draw correctly.
function groupArrangementCard(ctx, entries) {
  const { actions, busy } = ctx;
  const names = entries
    .filter((entry) => entry.type === "live")
    .map((entry) => displayName(entry.computer));
  return card({
    id: "home-group-arrangement",
    title: "Sharing with several computers",
    description: `MonHop shares one keyboard and mouse across ${joinNames(names)}.`,
    actions: [
      button("Arrange displays", {
        variant: "outline",
        size: "sm",
        disabled: busy,
        focusKey: "home-group-arrange",
        onClick: actions.changeLayout,
      }),
    ],
  });
}

function joinNames(names) {
  if (names.length < 2) return names.join("");
  if (names.length === 2) return names.join(" and ");
  return `${names.slice(0, -1).join(", ")} and ${names.at(-1)}`;
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
        el("div", {
          className: "home-layout-heading",
          children: [
            el("span", { text: "Displays" }),
            button(saved ? "Change layout" : "Arrange displays", {
              variant: "outline",
              size: "sm",
              disabled: busy,
              focusKey: "home-change-layout",
              onClick: actions.changeLayout,
            }),
          ],
        }),
        swap(
          `${key}-layout`,
          saved
            ? createDashboardArrangement(activeComputer.setup, {
                local: platformLabel(localPlatform, true),
                peer: displayName(activeComputer),
                localPlatform,
                peerPlatform: activeComputer.platform,
                motionKey: `${key}-layout`,
                transitionName: "active-arrangement",
                compactLegend: true,
                hideCaption: true,
              })
            : note("No layout yet. Arrange the displays once to start sharing input."),
          saved ? "layout" : "none",
          { block: true },
        ),
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
    activeComputer.setup?.saved === true ? note(DRAWN_DISPLAYS_NOTE) : null,
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
