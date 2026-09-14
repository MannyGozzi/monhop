import { canCheck, canInstall, installHint, updatesStatusText } from "./updates-model.mjs";
import {
  autostartDescription,
  autostartOpenLabel,
  autostartStatusText,
  showAutostartOpenSettings,
} from "./autostart-model.mjs";
import { button, card, clear, el, note, switchRow } from "./dom.mjs";

export function renderSettings(nodes, ctx) {
  clear(nodes.settingsContent);
  nodes.settingsContent.append(updatesCard(ctx), startupCard(ctx), aboutCard(ctx));
}

function updatesCard(ctx) {
  const { core, updates, actions } = ctx;
  const { view, pending } = updates;
  const hint = installHint(view);
  const children = [
    switchRow("Install updates automatically", {
      checked: view.automatic,
      description:
        "Checks github.com for a signed MonHop build a few times a day and downloads it. " +
        "It is installed when you quit MonHop or press Restart to update, never while sharing runs.",
      disabled: pending || !core,
      id: "settings-updates-automatic",
      onChange: actions.setUpdatesAutomatic,
    }),
    el("p", { className: "note", id: "settings-updates-status", text: updatesStatusText(view) }),
  ];
  if (view.phase === "downloading")
    children.push(
      el("div", {
        className: "progress-bar",
        id: "settings-updates-progress",
        children: [
          el("div", {
            className: "progress-bar-fill",
            attrs: { style: `width: ${view.progressPercent ?? 0}%` },
          }),
        ],
      }),
    );
  if (hint) children.push(note(hint, "danger"));
  return card({
    id: "settings-updates",
    title: "Updates",
    children,
    actions: [
      button("Check now", {
        id: "settings-updates-check",
        variant: "outline",
        disabled: pending || !canCheck(view),
        busy: pending,
        onClick: actions.checkForUpdates,
      }),
      button("Restart to update", {
        id: "settings-updates-install",
        disabled: pending || !canInstall(view),
        busy: pending,
        onClick: actions.installUpdate,
      }),
    ],
  });
}

function startupCard(ctx) {
  const { core, platform, autostart, actions } = ctx;
  const { view, pending } = autostart;
  const children = [
    switchRow("Start MonHop when you log in", {
      checked: view.enabled,
      description: autostartDescription(platform),
      disabled: pending || !core,
      id: "settings-autostart",
      onChange: actions.setAutostart,
    }),
    el("p", {
      className: "note",
      id: "settings-autostart-status",
      text: autostartStatusText(view),
    }),
  ];
  const actionButtons = [];
  if (showAutostartOpenSettings(view))
    actionButtons.push(
      button(autostartOpenLabel(platform), {
        id: "settings-autostart-open",
        variant: "outline",
        disabled: pending,
        busy: pending,
        onClick: actions.openAutostartSettings,
      }),
    );
  return card({
    id: "settings-startup",
    title: "Startup",
    children,
    actions: actionButtons,
  });
}

function aboutCard(ctx) {
  const { view } = ctx.updates;
  const { actions } = ctx;
  return card({
    id: "settings-about",
    title: "About",
    children: [
      el("p", { text: `MonHop ${view.currentVersion} (${view.buildCommit})` }),
      note("Free software under the GNU GPL v3 or later."),
    ],
    actions: [
      button("Source code", {
        id: "settings-about-source",
        variant: "outline",
        onClick: () => actions.openLink("source"),
      }),
      button("Release notes", {
        id: "settings-about-releases",
        variant: "outline",
        onClick: () => actions.openLink("releases"),
      }),
      button("Support MonHop", {
        id: "settings-about-support",
        iconName: "heart",
        onClick: () => actions.openLink("support"),
      }),
    ],
  });
}
