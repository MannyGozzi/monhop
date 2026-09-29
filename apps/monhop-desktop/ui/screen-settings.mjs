import { canCheck, canInstall, installHint, updatesStatusText } from "./updates-model.mjs";
import {
  autostartDescription,
  autostartOpenLabel,
  autostartStatusText,
  showAutostartOpenSettings,
} from "./autostart-model.mjs";
import {
  CLIPBOARD_NEEDS_CONNECTION,
  clipboardAccessNotice,
  clipboardNoticeText,
  clipboardPeerLines,
  clipboardStatusText,
  lastTransferText,
  normalizeClipboardView,
} from "./clipboard-model.mjs";
import { button, card, clear, el, note, rows, switchRow } from "./dom.mjs";

export function renderSettings(nodes, ctx) {
  clear(nodes.settingsContent);
  nodes.settingsContent.append(
    ...[updatesCard(ctx), clipboardCard(ctx), startupCard(ctx), aboutCard(ctx)].filter(Boolean),
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

// Hidden until app.js wires `state.clipboard = { view, pending }` from clipboard_status / the
// "clipboard" event, so an app.js that predates this card keeps rendering exactly as it did.
function clipboardCard(ctx) {
  const { clipboard, computers, actions } = ctx;
  if (!clipboard) return null;
  const view = normalizeClipboardView(clipboard.view);
  const canToggle = typeof actions.setClipboardEnabled === "function";
  const children = [
    rows([
      switchRow("Share the clipboard", {
        checked: view.enabled,
        description:
          "Off until you turn it on. Text and images you copy go to connected computers that " +
          "also have it on. Files never. Password-manager items are skipped. Stays on your " +
          "network.",
        disabled: clipboard.pending || !ctx.core || !canToggle,
        id: "settings-clipboard-enable",
        onChange: actions.setClipboardEnabled,
      }),
    ]),
    el("p", {
      className: "note",
      id: "settings-clipboard-status",
      text: clipboardStatusText(view),
    }),
  ];
  for (const line of clipboardPeerLines(view, computers)) children.push(note(line));
  const accessNotice = clipboardAccessNotice(view);
  if (accessNotice) children.push(note(accessNotice, "danger"));
  const skipNotice = clipboardNoticeText(view);
  if (skipNotice) children.push(note(skipNotice));
  const lastText = lastTransferText(view);
  if (lastText) children.push(note(lastText));
  children.push(note(CLIPBOARD_NEEDS_CONNECTION));
  return card({ id: "settings-clipboard", title: "Clipboard", children });
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
