import { canCheck, canInstall, installHint, updatesStatusText } from "./updates-model.mjs";
import {
  autostartDescription,
  autostartOpenLabel,
  autostartStatusText,
  showAutostartOpenSettings,
} from "./autostart-model.mjs";
import { ISLAND_DESCRIPTION, ISLAND_SWITCH_LABEL, ISLAND_TITLE } from "./island-model.mjs";
import { presence } from "./accordion.mjs";
import { button, card, clear, el, note, progressBar, rows, switchRow } from "./dom.mjs";

export function renderSettings(nodes, ctx) {
  clear(nodes.settingsContent);
  nodes.settingsContent.append(
    ...[updatesCard(ctx), startupCard(ctx), islandCard(ctx), aboutCard(ctx)].filter(Boolean),
  );
}

function updatesCard(ctx) {
  const { core, updates, actions } = ctx;
  const { view, pending } = updates;
  const hint = installHint(view);
  const children = [
    rows([
      switchRow("Install updates automatically", {
        checked: view.automatic,
        description:
          "Off until you enable it. Checks github.com and downloads signed updates only while sharing is paused. " +
          "Install by quitting MonHop or pressing Restart to update.",
        disabled: pending || !core,
        id: "settings-updates-automatic",
        onChange: actions.setUpdatesAutomatic,
      }),
    ]),
    note(updatesStatusText(view)),
    // Both come and go with the phase; each glides its own height so the cards below never jump.
    presence(
      "settings-updates-progress",
      view.phase === "downloading"
        ? progressBar("settings-updates-progress", (view.progressPercent ?? 0) / 100)
        : null,
    ),
    presence("settings-updates-hint", hint ? note(hint, "danger") : null),
  ];
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
    rows([
      switchRow("Start MonHop when you log in", {
        checked: view.enabled,
        description: autostartDescription(platform),
        disabled: pending || !core,
        id: "settings-autostart",
        onChange: actions.setAutostart,
      }),
    ]),
    note(autostartStatusText(view)),
    presence(
      "settings-autostart-open",
      showAutostartOpenSettings(view)
        ? el("div", {
            className: "card-actions",
            children: [
              button(autostartOpenLabel(platform), {
                id: "settings-autostart-open",
                variant: "outline",
                disabled: pending,
                busy: pending,
                onClick: actions.openAutostartSettings,
              }),
            ],
          })
        : null,
    ),
  ];
  return card({ id: "settings-startup", title: "Startup", children });
}

// Notched Macs only: the context is undefined wherever no display can show the island.
function islandCard(ctx) {
  const { core, island, actions } = ctx;
  if (!island) return null;
  const { view, pending } = island;
  return card({
    id: "settings-island",
    title: ISLAND_TITLE,
    children: [
      rows([
        switchRow(ISLAND_SWITCH_LABEL, {
          checked: view.enabled,
          description: ISLAND_DESCRIPTION,
          disabled: pending || !core,
          id: "settings-island-enabled",
          onChange: actions.setIslandEnabled,
        }),
      ]),
      presence("settings-island-error", view.error ? note(view.error, "danger") : null),
    ],
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
