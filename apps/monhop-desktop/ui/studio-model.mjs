// Pure logic behind the Studio shell: what the sidebar has selected, the rows it draws, what the
// toolbar offers and how the inspector is presented. app.js and sidebar.mjs only draw these answers.
import { computerStatus } from "./computer-status.mjs";
import { displayName } from "./computers-model.mjs";
import { canSelectInterface, interfaceAssessment, networkLabel } from "./model.mjs";
import { PAIRING_PROGRESS, platformLabel } from "./pairing-model.mjs";

// Below this window width the inspector stops sharing the row with the canvas and becomes a sheet.
export const INSPECTOR_OVERLAY_BELOW = 1060;

export const LOCAL = Object.freeze({ kind: "local" });
export const NEW = Object.freeze({ kind: "new" });
export const SETTINGS = Object.freeze({ kind: "settings" });
const KINDS = new Set(["local", "computer", "new", "settings"]);

export function computerSelection(fingerprint) {
  return { kind: "computer", fingerprint };
}

export function selectionKey(selection) {
  if (selection?.kind === "computer") return `computer:${selection.fingerprint}`;
  return KINDS.has(selection?.kind) ? selection.kind : "local";
}

export function selectionFromKey(key) {
  if (typeof key !== "string") return null;
  if (key.startsWith("computer:")) {
    const fingerprint = key.slice("computer:".length);
    return fingerprint ? computerSelection(fingerprint) : null;
  }
  return { local: LOCAL, new: NEW, settings: SETTINGS }[key] ?? null;
}

export function sameSelection(a, b) {
  return selectionKey(a) === selectionKey(b);
}

// With nothing chosen: a live pairing, then the computer a pairing just added, then the computer in
// use, then the first paired one, and This Mac when nothing is paired.
export function defaultSelection({
  fingerprints = [],
  active = null,
  pairingLive = false,
  paired = null,
}) {
  if (pairingLive) return NEW;
  for (const fingerprint of [paired, active, fingerprints[0]])
    if (fingerprint && fingerprints.includes(fingerprint)) return computerSelection(fingerprint);
  return LOCAL;
}

// The user's own pick sticks until its computer disappears. A computer a pairing just added is
// not in the list until the next read, so until then the pairing stays on screen.
export function resolveSelection({ chosen = null, ...context }) {
  const fingerprints = context.fingerprints ?? [];
  if (chosen?.kind === "computer") {
    if (fingerprints.includes(chosen.fingerprint)) return chosen;
    if (chosen.fingerprint && chosen.fingerprint === context.paired) return NEW;
    return defaultSelection(context);
  }
  if (KINDS.has(chosen?.kind)) return chosen;
  return defaultSelection(context);
}

// What the inspector holds for a selection; Settings takes the whole workspace instead.
export function inspectorKind(selection) {
  const kind = selection?.kind;
  return kind === "settings" || !KINDS.has(kind) ? null : kind;
}

// Docked beside the canvas when the window is wide enough, otherwise a sheet over it that the user
// can close; picking a row opens it again.
export function inspectorPresentation({ width, kind, dismissed = false }) {
  const mode = width < INSPECTOR_OVERLAY_BELOW ? "overlay" : "docked";
  return { mode, open: kind !== null && (mode === "docked" || !dismissed) };
}

// The verdict's own colors (`.overview` in styles.css), so the row and the inspector never disagree.
const LOCAL_TONES = { done: "connected", fail: "error", wait: "checking" };

// This Mac's row: the session's state while one runs, otherwise how far getting ready has come.
export function localRowStatus({ verdict, session = null }) {
  if (session && ["sharing", "reconnecting", "editing"].includes(session.key))
    return { label: session.label, tone: session.tone };
  return {
    label: verdict?.label ?? "Not checked yet",
    tone: LOCAL_TONES[verdict?.tone] ?? "neutral",
  };
}

const PAIRING_ROW_LABELS = {
  closed: "Opening pairing",
  identity: "Needs an identity",
  choose: "Show or enter a code",
  enter: "Type the code",
  show: "Waiting for the code",
  expired: "The code expired",
  paired: "Paired",
  error: "Pairing did not finish",
};
const PROGRESS_ROW_LABELS = {
  ...Object.fromEntries(PAIRING_PROGRESS.map((step) => [step.key, step.title])),
  stopping: "Stopping",
};

export function pairingRowLabel(step, phase) {
  if (step === "progress") return PROGRESS_ROW_LABELS[phase] ?? "Working";
  return PAIRING_ROW_LABELS[step] ?? PAIRING_ROW_LABELS.choose;
}

// The computer a finished exchange added, while its result is on screen: the pairing row becomes it.
export function pairedOnScreen({ selection, pairingStep, view = null }) {
  if (selection?.kind !== "new" || pairingStep !== "paired" || !view?.peerFingerprint) return null;
  return { fingerprint: view.peerFingerprint, platform: view.peerPlatform, badge: view.badge };
}

// Every sidebar row top to bottom: This Mac, each paired computer with its own state, and the row
// that adds a computer, which becomes the selected New computer while a pairing is on screen. Once
// that pairing finishes (`paired`), the same row turns into the computer it added, listed once:
// `adopt` hands it the add row's node, and `select` keeps the exchange selected until Done.
export function sidebarRows({
  computers,
  sharingView = null,
  active = null,
  localName,
  localPlatform,
  localStatus,
  selection,
  pairingStep = "choose",
  pairingPhase = null,
  paired = null,
}) {
  const key = selectionKey(selection);
  const rows = [
    {
      key: "local",
      select: "local",
      kind: "local",
      title: localName,
      label: localStatus.label,
      tone: localStatus.tone,
      platform: localPlatform,
      badge: null,
      selected: key === "local",
    },
  ];
  const items = computers?.items ?? [];
  for (const computer of items) {
    if (computer.fingerprint === paired?.fingerprint) continue;
    const status = computerStatus(computer, sharingView, active, localName);
    const rowKey = `computer:${computer.fingerprint}`;
    rows.push({
      key: rowKey,
      select: rowKey,
      kind: "computer",
      fingerprint: computer.fingerprint,
      title: displayName(computer),
      label: status.label,
      tone: status.tone,
      platform: computer.platform,
      badge: computer.badge ?? null,
      selected: key === rowKey,
    });
  }
  if (paired && key === "new") {
    const listed = items.find((item) => item.fingerprint === paired.fingerprint) ?? null;
    rows.push({
      key: `computer:${paired.fingerprint}`,
      select: "new",
      adopt: "new",
      kind: "computer",
      fingerprint: paired.fingerprint,
      title: listed ? displayName(listed) : platformLabel(paired.platform),
      label: PAIRING_ROW_LABELS.paired,
      tone: "connected",
      platform: listed?.platform ?? paired.platform ?? null,
      badge: paired.badge ?? listed?.badge ?? null,
      badgeShown: true,
      selected: true,
    });
    return rows;
  }
  const adding = key === "new";
  rows.push({
    key: "new",
    select: "new",
    kind: "new",
    title: adding ? "New computer" : "Add a computer",
    label: adding ? pairingRowLabel(pairingStep, pairingPhase) : "Show or enter a code",
    tone: adding && ["show", "progress"].includes(pairingStep) ? "checking" : "neutral",
    platform: null,
    badge: null,
    selected: adding,
  });
  return rows;
}

// The next or previous item of a list for the arrow keys, Home and End; the ends hold, never wrap.
export function listStep(count, index, key) {
  if (count < 1) return -1;
  const last = count - 1;
  const at = Math.min(Math.max(index, 0), last);
  if (key === "Home") return 0;
  if (key === "End") return last;
  if (key === "ArrowDown") return index < 0 ? 0 : Math.min(at + 1, last);
  if (key === "ArrowUp") return index < 0 ? last : Math.max(at - 1, 0);
  return -1;
}

// Arranging is its own canvas mode. A link that is up without a session has nothing applied yet,
// so it stays in the mode until Apply; one the user opened can be left with Done.
export function arrangeState({ connected = false, editing = false, requested = false }) {
  return { active: connected || editing || requested, leavable: editing || requested };
}

// The computer the canvas draws and arranges with: the selected one, else the one in use.
export function canvasFingerprint({ selection, active = null, fingerprints = [] }) {
  if (selection?.kind === "computer" && fingerprints.includes(selection.fingerprint))
    return selection.fingerprint;
  if (active && fingerprints.includes(active)) return active;
  return fingerprints[0] ?? null;
}

// Resume brings back the computer last in use; failing that the selected one, then the first.
export function resumeTarget({ lastActive = null, selection = null, fingerprints = [] }) {
  for (const fingerprint of [
    lastActive,
    selection?.kind === "computer" ? selection.fingerprint : null,
    fingerprints[0],
  ])
    if (fingerprint && fingerprints.includes(fingerprint)) return fingerprint;
  return null;
}

// The toolbar: Settings keeps only its title; the canvas offers Arrange, or Done once arranging can
// be left, and Pause while anything is switched on or Resume once a paired computer could come back.
export function toolbarState({
  selection,
  arrange,
  canArrange = false,
  enabledCount = 0,
  resume = null,
}) {
  const settings = selection?.kind === "settings";
  return {
    title: settings ? "Settings" : "Arrangement",
    canvasActions: !settings,
    arrange: arrange.active ? (arrange.leavable ? "done" : null) : canArrange ? "begin" : null,
    pause: enabledCount > 0 ? "pause" : resume ? "resume" : null,
  };
}

// What the clipboard switch in Settings means for one computer.
export function clipboardLine(view, fingerprint, name) {
  if (!view?.enabled) return "Clipboard sharing is off. Turn it on in Settings.";
  const peer = view.peers?.find((entry) => entry.fingerprint === fingerprint);
  if (!peer) return `Copied text and images reach ${name} while you share input with it.`;
  return peer.peerEnabled === true
    ? `Sharing the clipboard with ${name}.`
    : `Turn on clipboard sharing on ${name} too.`;
}

export const NETWORK_LOCKED = "Pause the computer in use before changing the network.";

// The network chip names the one connection MonHop may use, and is locked while a session runs.
// `name` and `address` are drawn apart, the address in mono; `text` reads them as one.
export function networkChip({
  selected = null,
  label = "",
  eligibleCount = 0,
  sessionActive = false,
}) {
  const name = selected ? label : "Choose a network";
  const address = selected?.address ?? "";
  const parts = { name, address, text: [name, address].filter(Boolean).join(" · ") };
  if (sessionActive) return { ...parts, disabled: true, hint: NETWORK_LOCKED };
  if (!eligibleCount)
    return {
      ...parts,
      disabled: true,
      hint: "No connected Wi-Fi or Ethernet network was found.",
    };
  return { ...parts, disabled: false, hint: "Both computers must be on this network." };
}

// The chip's menu: every network MonHop could use, the chosen one checked, then the ones it cannot
// use, greyed out with the reason.
export function networkMenuItems(interfaces = [], selectedId = null) {
  const usable = [];
  const unusable = [];
  for (const item of interfaces) {
    const entry = { id: item.id, label: networkLabel(item) };
    if (canSelectInterface(item))
      usable.push({
        ...entry,
        detail: [item.address || item.name, item.attachmentKnown ? "" : "Not recognized"]
          .filter(Boolean)
          .join(" · "),
        checked: item.id === selectedId,
        disabled: false,
      });
    else
      unusable.push({
        ...entry,
        detail: interfaceAssessment(item),
        checked: false,
        disabled: true,
      });
  }
  return [...usable, ...unusable];
}
