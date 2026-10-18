import {
  applySnapshot,
  autoSelectInterface,
  errorMessages,
  initialState,
  nativeCheckState,
  networkLabel,
  pairingOpenGate,
  selectInterface,
  selectedInterface,
  setupVerdict,
} from "./model.mjs";
import {
  applyPairingView,
  canStartPairing,
  canSubmitPairingCode,
  choosePairingMode as withPairingMode,
  codeTimeLeft,
  editPairingEntry,
  failPairingEntry,
  initialPairingState,
  isBusyPairing,
  isPairingAttemptLive,
  pairingFailure,
  pairingStep,
  platformLabel,
} from "./pairing-model.mjs";
import {
  COUNTDOWN_TEXT_ID,
  PAIRING_CODE_INPUT_ID,
  countdownText,
  pairingInspector,
} from "./screen-computers.mjs";
import {
  applySharingView,
  arrangementByName,
  beginPending,
  canApplySetup,
  canEditLayout,
  canLoadArrangement,
  canSaveArrangement,
  failPending,
  initialSharingState,
  initializeArrangement,
  isActionBusy,
  isBusySharing,
  isConnected,
  isCurrentPending,
  isEditingLayout,
  isLinkActive,
  isSessionActive,
  layoutForSave,
  loadArrangement,
  loadArrangementLayout,
  normalizeSharingView,
  resetArrangement as resetSharingArrangement,
  sameSharingView,
  setArrangement,
  setArrangements,
  setDisplayInUse,
  setMonitorSide,
  settlePending,
  shouldPollSharing,
} from "./sharing-model.mjs";
import {
  beginCopyFeedback,
  copyFeedbackFailure,
  copyFeedbackSuccess,
  emptyCopyFeedback,
  isCopyReplyCurrent,
} from "./copy-feedback-model.mjs";
import { accordionManager, glideWidth } from "./accordion.mjs";
import {
  canOpenPairingOnEntry,
  recordPairingOpenContext,
  selectedNetworkContextKey,
  shouldStartAutomaticSnapshot,
  snapshotCheckResult,
} from "./auto-check.mjs";
import {
  beginComputerArrangements,
  computerArrangements,
  coverComputersLoad,
  displayName,
  failComputerArrangements,
  findComputer,
  finishComputersLoad,
  followSetupRevision,
  initialComputerArrangements,
  initialComputers,
  initialComputersLoad,
  normalizeComputers,
  pruneComputerArrangements,
  requestComputersLoad,
  setComputerArrangements,
} from "./computers-model.mjs";
import { activeStatus } from "./computer-status.mjs";
import {
  clearForgetFor,
  keepForgetArmed,
  keepForgetListed,
  loadGate,
  pressForget,
  resolveEnabledList,
} from "./computer-card-model.mjs";
import { clipboardContext, invokeSetComputerEnabled, pollFingerprint } from "./app-actions.mjs";
import { autoscrollContext } from "./autoscroll-model.mjs";
import { swipeContext } from "./swipe-model.mjs";
import { controlAsCommandContext } from "./keyboard-model.mjs";
import { forgetArrangementMotion } from "./dashboard-arrangement.mjs";
import {
  applyDimmingView,
  beginDimming,
  clampLevel,
  failDimming,
  initialDimming,
} from "./dimming-model.mjs";
import { setSlotPresent } from "./header-motion.mjs";
import {
  arrangeState,
  canvasFingerprint,
  computerSelection,
  inspectorKind,
  inspectorPresentation,
  localRowStatus,
  networkChip,
  resolveSelection,
  resumeTarget,
  sameSelection,
  selectionFromKey,
  selectionKey,
  sidebarRows,
  toolbarState,
} from "./studio-model.mjs";
import {
  button,
  captureDims,
  clear,
  el,
  followDims,
  handOffFocus,
  handedOffFocus,
  icon,
  iconButton,
  nativeError,
  phaseLoops,
  reducedMotion,
  setLabel,
  swap,
  turnGlyph,
} from "./dom.mjs";
import { canCheck, canInstall, normalizeUpdatesView } from "./updates-model.mjs";
import { normalizeAutostartView } from "./autostart-model.mjs";
import { islandContext, normalizeIslandView } from "./island-model.mjs";
import { isSelectable, localInspector } from "./screen-ready.mjs";
import { computerInspector } from "./computer-card.mjs";
import { banner, renderCanvas } from "./screen-displays.mjs";
import { renderSettings } from "./screen-settings.mjs";
import { renderSidebar } from "./sidebar.mjs";

const core = window.__TAURI__?.core;
const nativeWindow = window.__TAURI__?.window;
const uiCheck = window.__MONHOP_UI_CHECK__ === true;
const nativePlatform = window.__MONHOP_PLATFORM__ ?? document.documentElement.dataset.platform;
const platform = ["macos", "windows"].includes(nativePlatform) ? nativePlatform : "other";
document.documentElement.dataset.platform = platform;

const SHARING_POLL_MS = 1000;
const THEMES = new Set(["system", "light", "dark"]);
const PAIRING_POLL_MS = 500;
const PAIRING_COUNTDOWN_TICK_MS = 250;
const LIST_UNREADABLE = "The computer list was not recognized.";

let state = initialState(Boolean(core?.invoke));
let pairing = initialPairingState();
let pairingGeneration = 0;
let pairingPending = false;
let pairingOperation = null;
let pairingPollingAllowed = false;
let pairingPollTimer = null;
let pairedKey = null;
// A pairing this session finished keeps its match badge on screen until the user moves on.
let pairingResultShown = false;
let pairingCountdownTimer = null;
let sharing = initialSharingState();
let sharingPollTimer = null;
let sharingStatusPending = false;
let dropCopyFeedback = emptyCopyFeedback();
let dropCopyRequest = 0;
let dimming = initialDimming();
// Undefined until clipboard_status succeeds or a "clipboard" event arrives, so a backend built
// before clipboard sharing landed leaves the section hidden rather than showing a broken one.
let clipboardView;
let clipboardPending = false;
// The Mac's mouse and trackpad switches: each view is undefined until its status answers.
const pointerSwitches = {
  autoscroll: { view: undefined, pending: false },
  swipe: { view: undefined, pending: false },
};
// Windows's keyboard switch, undefined until its status answers.
const controlAsCommand = { view: undefined, pending: false };
let theme = "system";
let themePending = false;
let dimmingRequest = 0;
let dimPreview = null;
let dimmingDrag = false;
let renderAfterDrag = false;
// The sidebar row the user picked; null follows the default. The harness may start on any row.
let chosen = selectionFromKey(window.MONHOP_SELECT);
let shownSelectionKey = null;
// The computer in use before Pause, which Resume brings back.
let lastActive = null;
// Arrange was pressed and the editor is on its way; the backend's own editing flag takes over.
let arrangeRequested = false;
// Below the overlay width the details sheet can be closed; picking a row opens it again.
let inspectorDismissed = false;
// Which drawer above the Apply bar is open: "saved", "use" or none.
let canvasDrawer = null;
let networkMenuOpen = false;
let updates = normalizeUpdatesView(null);
let updatesPending = false;
let autostart = normalizeAutostartView(null);
let autostartPending = false;
// Undefined until the Mac answers, so the section never shows on a platform that has no island.
let island;
let islandPending = false;
let computers = initialComputers();
let computersRequest = 0;
let computersLoad = initialComputersLoad();
let computersLoadFailure = "";
// Each computer's layout history, kept apart from `computers` so a `computers_load` poll never wipes it.
let computerArrangementsStore = initialComputerArrangements();
// The one Layouts row armed to forget, as { fingerprint, name }, and one read or forget per
// computer at a time so a reply that predates a newer one never lands.
let layoutForget = null;
const arrangementRequests = new Map();
// Lists due a background read that waits for the read or forget of their own still in flight.
const arrangementsRefreshAfter = new Set();
let renaming = null;
// What the user has typed into the name so far; polls re-render the field and must not erase it.
let renameDraft = null;
let renamePending = null;
let forgetConfirmed = null;
let snapshotCheck = { pending: false, automaticFailed: false, freshness: "Not checked yet." };
let refreshAfterReturn = false;
let autoPairingContexts = new Set();
let arrangementView = null;
let arrangementsRevision = null;
let arrangementsRetry = { at: 0, delay: 0 };

const nodes = {
  studio: document.querySelector("#studio"),
  sidebarList: document.querySelector("#sidebar-list"),
  sidebarCount: document.querySelector("#sidebar-count"),
  sidebarRefresh: document.querySelector("#sidebar-refresh"),
  sidebarSettings: document.querySelector("#sidebar-settings"),
  sidebarIndicator: document.querySelector("#sidebar-indicator"),
  sidebarVersion: document.querySelector("#sidebar-version"),
  pageTitle: document.querySelector("#page-title"),
  networkSlot: document.querySelector("#network-slot"),
  networkChip: document.querySelector("#network-chip"),
  networkChipDot: document.querySelector("#network-chip-dot"),
  networkChipLabel: document.querySelector("#network-chip-label"),
  networkMenu: document.querySelector("#network-menu"),
  arrangeSlot: document.querySelector("#arrange-slot"),
  pauseSlot: document.querySelector("#pause-slot"),
  windowControls: document.querySelector("#window-controls"),
  canvasPane: document.querySelector("#canvas-pane"),
  canvasBanners: document.querySelector("#canvas-banners"),
  canvasStage: document.querySelector("#canvas-stage"),
  canvasDock: document.querySelector("#canvas-dock"),
  statusBarDot: document.querySelector("#status-bar-dot"),
  statusBarText: document.querySelector("#status-bar-text"),
  inspector: document.querySelector("#inspector"),
  inspectorClose: document.querySelector("#inspector-close"),
  inspectorScroll: document.querySelector("#inspector-scroll"),
  inspectorContent: document.querySelector("#inspector-content"),
  settingsView: document.querySelector("#settings-view"),
  settingsScroll: document.querySelector("#settings-scroll"),
  settingsContent: document.querySelector("#settings-content"),
};
// Scrollers whose position a render keeps.
const scrollers = [
  nodes.inspectorScroll,
  nodes.settingsScroll,
  nodes.canvasStage,
  nodes.sidebarList,
];

for (const caption of nodes.windowControls.querySelectorAll("[data-window-action]")) {
  caption.addEventListener("pointerdown", (event) => event.stopPropagation());
  caption.addEventListener("dblclick", (event) => event.stopPropagation());
  caption.addEventListener("click", (event) => {
    event.stopPropagation();
    runWindowAction(caption.dataset.windowAction);
  });
}
nodes.windowControls.hidden = platform !== "windows";
accordionManager.mount();
const toolbarControls = createToolbarControls();
mountStaticGlyphs();

nodes.sidebarSettings.addEventListener("click", () => select("settings"));
nodes.inspectorClose.addEventListener("click", dismissInspector);
nodes.networkChip.addEventListener("click", () => setNetworkMenuOpen(!networkMenuOpen));
document.addEventListener("pointerdown", (event) => {
  if (networkMenuOpen && !nodes.networkSlot.contains(event.target)) setNetworkMenuOpen(false);
});
document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape") return;
  if (networkMenuOpen) {
    setNetworkMenuOpen(false);
    nodes.networkChip.focus({ preventScroll: true });
  } else if (nodes.inspector.dataset.mode === "overlay" && nodes.inspector.dataset.open === "true")
    dismissInspector();
});

// Only the sheet closes; a docked inspector always shows its selection.
function dismissInspector() {
  inspectorDismissed = true;
  render();
}
// The inspector turns into a sheet, or back, as the window crosses the overlay width.
window.addEventListener("resize", () => {
  if (nodes.inspector.dataset.mode !== presentInspector(inspectorKind(currentSelection())).mode)
    render();
});

window.addEventListener("blur", () => {
  refreshAfterReturn = true;
});
window.addEventListener("focus", onReturn);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "hidden") {
    refreshAfterReturn = true;
    return;
  }
  if (document.visibilityState === "visible") onReturn();
});

render();
void loadTheme();
if (!uiCheck) {
  void loadComputers();
  void requestAutomaticSnapshot("startup");
  void refreshSharingStatus();
  void loadDimming();
  listenDimming();
  void loadClipboard();
  listenClipboard();
  if (platform === "macos") void loadPointerSwitches();
  if (platform === "windows") void loadControlAsCommand();
  void loadUpdatesStatus();
  void loadAutostartStatus();
  if (platform === "macos") void loadIslandStatus();
  listenUpdates();
}

function onReturn() {
  if (uiCheck || !refreshAfterReturn) return;
  refreshAfterReturn = false;
  if (showsReadiness(currentSelection())) void requestAutomaticSnapshot("focus");
  void refreshSharingStatus();
  void loadComputers();
}

// ---------- selection ----------

function justPaired() {
  return pairingResultShown && pairing.view?.phase === "paired"
    ? pairing.view.peerFingerprint
    : null;
}

function currentSelection() {
  return resolveSelection({
    chosen,
    fingerprints: computers.items.map((item) => item.fingerprint),
    active: activeFingerprint(),
    pairingLive: isPairingAttemptLive(pairing),
    paired: justPaired(),
  });
}

// This Mac and the pairing exchange are where readiness and the pairing entry are on screen.
function showsReadiness(selection) {
  return selection.kind === "local" || selection.kind === "new";
}

// Returns the pairing it opened on the way into the New computer row, if any.
function select(key) {
  const next = selectionFromKey(key);
  if (!next) return null;
  setNetworkMenuOpen(false);
  inspectorDismissed = false;
  const previous = currentSelection();
  chosen = next;
  const opening = sameSelection(previous, next) ? null : enterSelection(previous, next);
  render();
  return opening;
}

function enterSelection(previous, next) {
  // A shown code or a fresh match badge belongs to the exchange; moving on abandons it.
  const paired = justPaired();
  if (
    previous.kind === "new" ||
    (paired && next.kind !== "new" && !sameSelection(next, computerSelection(paired)))
  )
    leavePairing();
  if (next.kind === "computer") void loadComputers();
  if (next.kind === "settings") {
    void loadUpdatesStatus();
    void loadAutostartStatus();
    if (platform === "macos") void loadIslandStatus();
  }
  if (showsReadiness(next)) void requestAutomaticSnapshot("entry");
  if (next.kind === "local") openPairingOnExplicitEntry();
  return next.kind === "new" ? enterPairing() : null;
}

// A shown code or a running handshake holds the sharing port; leaving the exchange abandons it so
// the supervisor can reconnect. Pairing again means a new code.
function leavePairing() {
  pairingResultShown = false;
  pairing = withPairingMode(pairing, null);
  if (isPairingAttemptLive(pairing)) void cancelPairing();
}

// ---------- render ----------

function captureInteraction() {
  const active = document.activeElement;
  const focused =
    active?.id || active?.dataset.focusKey
      ? {
          id: active.dataset.focusKey ? null : active.id || null,
          focusKey: active.dataset.focusKey || null,
          start: typeof active.selectionStart === "number" ? active.selectionStart : null,
          end: typeof active.selectionEnd === "number" ? active.selectionEnd : null,
        }
      : null;
  return { scroll: scrollers.map((node) => node.scrollTop), focused };
}

function restoreInteraction(interaction, { resetInspector }) {
  scrollers.forEach((node, index) => {
    node.scrollTop = interaction.scroll[index];
  });
  if (resetInspector) nodes.inspectorScroll.scrollTop = 0;
  if (!interaction.focused) return;
  const focused = interaction.focused.id
    ? document.getElementById(interaction.focused.id)
    : [...document.querySelectorAll("[data-focus-key]")].find(
        (node) => node.dataset.focusKey === interaction.focused.focusKey,
      );
  // A control inside a pane that closed cannot take focus back; one inside a block that closed
  // goes where that block handed its focus.
  if (!focused || focused.disabled) return;
  const target = focused.closest("[inert]") ? handedOffFocus(focused) : focused;
  if (!target) return;
  target.focus({ preventScroll: true });
  if (
    target === focused &&
    interaction.focused.start !== null &&
    typeof focused.setSelectionRange === "function"
  )
    focused.setSelectionRange(interaction.focused.start, interaction.focused.end);
}

// Which computer MonHop shares input with. The sharing view is polled every second and the
// computer list is not, so the view wins whenever it has an answer.
function activeFingerprint() {
  return sharing.view?.active ?? computers.active;
}

function peerName(activeComputer) {
  if (activeComputer) return displayName(activeComputer);
  const candidate = findComputer(computers, pairing.view?.peerFingerprint ?? null);
  if (candidate) return displayName(candidate);
  if (pairing.view?.peerPlatform) return platformLabel(pairing.view.peerPlatform);
  if (sharing.view?.peerPlatform) return platformLabel(sharing.view.peerPlatform);
  return "the other computer";
}

function presentInspector(kind) {
  return inspectorPresentation({ width: window.innerWidth, kind, dismissed: inspectorDismissed });
}

// The shell's own state: what is selected, what the toolbar offers and what the canvas draws.
function studioContext(status, active) {
  const selection = currentSelection();
  const fingerprints = computers.items.map((item) => item.fingerprint);
  const localPlatform = state.snapshot?.platform ?? platform;
  const localName = platformLabel(localPlatform, true);
  const enabled = resolveEnabledList(sharing.view, computers, active);
  const arrange = arrangeState({
    connected: isConnected(sharing),
    editing: isEditingLayout(sharing),
    requested: arrangeRequested,
  });
  const canvasPrint = canvasFingerprint({ selection, active, fingerprints });
  const canArrange =
    Boolean(canvasPrint) && canEditLayout(sharing, state.selectedInterfaceId) && !controlsBusy();
  const resume = resumeTarget({ lastActive, selection, fingerprints });
  return {
    selection,
    selectionKey: selectionKey(selection),
    view: selection.kind === "settings" ? "settings" : "canvas",
    inspectorKind: inspectorKind(selection),
    rows: sidebarRows({
      computers,
      sharingView: sharing.view,
      active,
      localName,
      localPlatform,
      localStatus: localRowStatus({
        verdict: setupVerdict(state, { checking: snapshotCheck.pending }),
        session: status,
      }),
      selection,
      pairingStep: pairingStep(pairing),
      pairingPhase: pairing.view?.phase ?? null,
    }),
    canvasComputer: findComputer(computers, canvasPrint),
    canArrange,
    arrange,
    toolbar: toolbarState({ selection, arrange, canArrange, enabledCount: enabled.length, resume }),
    resume,
  };
}

function context() {
  const active = activeFingerprint();
  const activeComputer = findComputer(computers, active);
  const status = activeStatus({
    computers,
    sharingView: sharing.view,
    active,
    enabled: computers.enabled,
    nativeAvailable: state.nativeAvailable,
    localName: platformLabel(platform, true),
  });
  const studio = studioContext(status, active);
  return {
    core: Boolean(core?.invoke),
    platform,
    uiCheck,
    state,
    pairing,
    pairingResultShown,
    sharing,
    computers,
    computerArrangements: computerArrangementsStore,
    layoutForget,
    active,
    activeComputer,
    status,
    studio,
    arrange: studio.arrange,
    toolbar: studio.toolbar,
    canvasDrawer,
    snapshotCheck,
    pairingPending,
    pairingOperation,
    computersLoadPending: computersLoad.running,
    computersLoadFailure,
    renaming,
    renamePending,
    renameDraft,
    forgetConfirmed,
    now: Date.now(),
    dropCopyFeedback,
    dimming,
    clipboard: clipboardContext(clipboardView, clipboardPending),
    autoscroll: autoscrollContext(
      platform,
      pointerSwitches.autoscroll.view,
      pointerSwitches.autoscroll.pending,
    ),
    swipe: swipeContext(platform, pointerSwitches.swipe.view, pointerSwitches.swipe.pending),
    controlAsCommand: controlAsCommandContext(
      platform,
      controlAsCommand.view,
      controlAsCommand.pending,
    ),
    updates: { view: updates, pending: updatesPending },
    autostart: { view: autostart, pending: autostartPending },
    island: islandContext(platform, island, islandPending),
    theme,
    themePending,
    busy: controlsBusy(),
    peerName: peerName(activeComputer),
    gate: pairingOpenGate(state),
    actions: {
      select,
      startPairing,
      arrangeWith,
      leaveArrange,
      pauseSharing,
      resumeSharing,
      toggleCanvasDrawer,
      setTheme,
      refreshSnapshot,
      requestPermissions,
      requestWifiPermission,
      openSettings,
      chooseInterface,
      openPairing,
      beginPairing,
      dismissPairing,
      createPairingIdentity,
      choosePairingMode,
      showPairingCode,
      editPairingCode,
      submitPairingCode,
      cancelPairing,
      copyLastDrop,
      setDimmingEnabled,
      previewDimLevel,
      setDimLevel,
      toggleDimming,
      beginDimDrag,
      endDimDrag,
      useComputer,
      setComputerEnabled,
      setClipboardEnabled,
      setAutoscrollEnabled,
      setSwipeEnabled,
      setControlAsCommand,
      startRename,
      draftRename,
      cancelRename,
      renameComputer,
      confirmForget,
      forgetComputer,
      loadComputers,
      beginLayoutEdit,
      endLayoutEdit,
      changeLayout,
      dismissDisplayNotice,
      setControl,
      applySetup,
      commitArrangement,
      showMonitorOn,
      useDisplay,
      resetArrangement,
      loadSavedArrangement,
      saveArrangement,
      deleteArrangement,
      loadComputerArrangements,
      loadComputerArrangement,
      pressLayoutForget,
      forgetComputerArrangement,
      revealLogFile,
      hideToTray,
      setUpdatesAutomatic,
      checkForUpdates,
      installUpdate,
      setAutostart,
      openAutostartSettings,
      setIslandEnabled,
      openLink,
      setArrangementView(view) {
        arrangementView = view;
      },
    },
  };
}

function render() {
  // A slider mid-drag must keep its element; the render runs when the pointer lets go.
  if (dimmingDrag) {
    renderAfterDrag = true;
    return;
  }
  const interaction = captureInteraction();
  const dims = captureDims(nodes.studio);
  // The canvas hands back the same editor across polls; only a view it dropped is torn down.
  const previousArrangementView = arrangementView;
  arrangementView = null;
  // The arrange request has done its job once the backend reports the edit.
  if (arrangeRequested && isEditingLayout(sharing)) arrangeRequested = false;
  lastActive = activeFingerprint() ?? lastActive;
  const ctx = context();
  const { studio } = ctx;
  // The editor needs the whole canvas, so a sheet over it steps aside as arranging starts.
  if (studio.arrange.active && nodes.studio.dataset.arranging !== "true") inspectorDismissed = true;
  const selectionChanged = shownSelectionKey !== null && shownSelectionKey !== studio.selectionKey;
  shownSelectionKey = studio.selectionKey;
  nodes.studio.dataset.checkState = nativeCheckState(state);
  nodes.studio.dataset.selection = studio.selection.kind;
  ctx.pageAlert = pageAlertNode(ctx);
  renderShell(ctx);
  renderToolbar(ctx);
  renderSidebar(nodes, ctx);
  renderCanvas(nodes, ctx);
  renderInspector(ctx);
  renderSettings(nodes, ctx);
  followDims(nodes.studio, dims);
  phaseLoops();
  restoreInteraction(interaction, { resetInspector: selectionChanged });
  if (previousArrangementView && previousArrangementView !== arrangementView)
    previousArrangementView.destroy();
  syncArrangements();
  syncPairingCountdown();
}

// Settings takes over the canvas and inspector; it rises in over them, and they come back as it leaves.
function renderShell({ studio }) {
  const settings = studio.view === "settings";
  const was = nodes.studio.dataset.view;
  nodes.studio.dataset.view = studio.view;
  nodes.sidebarVersion.textContent = updates.currentVersion ? `v${updates.currentVersion}` : "";
  if (was === studio.view) return;
  if (!settings) handOffFocus(nodes.settingsView, nodes.sidebarSettings);
  nodes.settingsView.inert = !settings;
  nodes.settingsView.dataset.open = String(settings);
  nodes.canvasPane.inert = settings;
  nodes.inspector.toggleAttribute("data-covered", settings);
  if (settings) nodes.settingsScroll.scrollTop = 0;
}

function renderInspector(ctx) {
  const { studio } = ctx;
  const kind = studio.inspectorKind;
  const computer =
    kind === "computer" ? findComputer(computers, studio.selection.fingerprint) : null;
  const content =
    kind === "local"
      ? localInspector(ctx)
      : kind === "new"
        ? pairingInspector(ctx)
        : computer
          ? computerInspector(ctx, computer)
          : null;
  const presentation = presentInspector(kind);
  nodes.inspector.dataset.mode = presentation.mode;
  nodes.studio.dataset.inspector = presentation.mode;
  const open = presentation.open && studio.view === "canvas";
  if (nodes.inspector.dataset.open !== String(open)) {
    if (!open) handOffFocus(nodes.inspector, nodes.sidebarIndicator.parentElement);
    nodes.inspector.dataset.open = String(open);
  }
  nodes.inspector.inert = !open;
  nodes.inspectorClose.hidden = presentation.mode !== "overlay";
  clear(nodes.inspectorContent);
  if (content)
    nodes.inspectorContent.append(swap("inspector", content, studio.selectionKey, { block: true }));
}

// The page alert sits above whichever pane is on screen. A pairing or arranging message that its
// own pane already shows is left out here.
function pageAlertNode(ctx) {
  const { studio, arrange } = ctx;
  const stateMessages = errorMessages(state);
  const messages = [...stateMessages];
  const pairingOnScreen = studio.view === "canvas" && studio.selection.kind === "new";
  const displaysOnScreen = studio.view === "canvas" && arrange.active && isConnected(sharing);
  if (pairing.view?.phase === "error" && pairing.view.message && !pairingOnScreen)
    messages.push(pairing.view.message);
  if (pairing.message && !pairingOnScreen) messages.push(pairing.message);
  if (sharing.message && !displaysOnScreen) messages.push(sharing.message);
  if (computersLoadFailure) messages.push(computersLoadFailure);
  const unique = [...new Set(messages.filter(Boolean))];
  if (!unique.length) return null;
  const failure =
    stateMessages.length ||
    computersLoadFailure ||
    pairing.message ||
    pairing.view?.phase === "error" ||
    sharing.view?.phase === "error";
  const actions = [];
  if (computersLoadFailure)
    actions.push(
      iconButton({
        id: "computers-retry",
        label: "Read the paired computers again",
        disabled: computersLoad.running,
        busy: computersLoad.running,
        size: "sm",
        variant: "ghost",
        onClick: loadComputers,
      }),
    );
  if (stateMessages.length || sharing.message)
    actions.push(
      iconButton({
        id: "dismiss-alert",
        label: "Dismiss",
        iconName: "x",
        variant: "ghost",
        size: "sm",
        onClick: () => {
          state = { ...state, messages: [] };
          sharing = { ...sharing, message: "" };
          render();
        },
      }),
    );
  return banner({
    id: "page-alert",
    tone: failure ? "danger" : "notice",
    iconName: "triangle-alert",
    body: unique.join(" "),
    actions,
  });
}

// ---------- toolbar ----------

function mountStaticGlyphs() {
  document.querySelector("#sidebar-settings-glyph").replaceChildren(icon("settings", 15));
  document.querySelector("#network-chip-chevron").replaceChildren(icon("chevron-right", 12));
  document.querySelector("#status-bar-escape-glyph").replaceChildren(icon("keyboard", 13));
  const refresh = iconButton({
    id: "computers-refresh",
    label: "Check the paired computers again",
    variant: "ghost",
    size: "sm",
    onClick: loadComputers,
  });
  nodes.sidebarRefresh.replaceChildren(refresh);
}

// The toolbar's buttons outlive renders, so hover, focus and a label mid-change carry on.
function createToolbarControls() {
  // Labels are elements, so setLabel can fade a new word in.
  const arrange = button("", {
    variant: "secondary",
    size: "sm",
    iconName: "monitor",
    focusKey: "toolbar-arrange",
    onClick: () => (toolbarControls.arrangeMode === "done" ? leaveArrange() : arrangeWith()),
  });
  arrange.id = "toolbar-arrange";
  arrange.append(el("span", { className: "toolbar-label", text: "Arrange" }));
  const pause = button("", {
    variant: "secondary",
    size: "sm",
    focusKey: "toolbar-pause",
    onClick: () => (toolbarControls.pauseMode === "resume" ? resumeSharing() : pauseSharing()),
  });
  pause.id = "toolbar-pause";
  // Both glyphs are drawn and the mode cross-fades between them: bars to pause, a triangle to resume.
  pause.prepend(
    el("span", {
      className: "pause-glyph",
      attrs: { "aria-hidden": "true" },
      children: [el("span", { className: "pause-bars" }), icon("play", 12)],
    }),
    el("span", { className: "toolbar-label", text: "Pause sharing" }),
  );
  nodes.arrangeSlot.replaceChildren(arrange);
  nodes.pauseSlot.replaceChildren(pause);
  return { arrange, pause, arrangeMode: null, pauseMode: null };
}

function renderToolbar(ctx) {
  const { studio, toolbar } = ctx;
  nodes.pageTitle.textContent = toolbar.title;
  const refresh = nodes.sidebarRefresh.firstElementChild;
  refresh.disabled = computersLoad.running || !core?.invoke;
  refresh.toggleAttribute("aria-busy", computersLoad.running);
  turnGlyph(refresh.querySelector(":scope > svg"), "computers-refresh", computersLoad.running);
  const canvas = toolbar.canvasActions;
  const busy = ctx.busy;
  const { arrange, pause } = toolbarControls;
  toolbarControls.arrangeMode = toolbar.arrange;
  if (toolbar.arrange) {
    arrange.title = toolbar.arrange === "done" ? "Done" : "Arrange";
    setLabel(arrange.lastChild, arrange.title);
    arrange.disabled = toolbar.arrange === "begin" && busy;
  }
  toolbarControls.pauseMode = toolbar.pause;
  if (toolbar.pause) {
    pause.title = toolbar.pause === "resume" ? "Resume" : "Pause sharing";
    setLabel(pause.lastChild, pause.title);
    pause.dataset.mode = toolbar.pause;
    pause.disabled = busy;
  }
  const movers = [nodes.networkSlot];
  setSlotPresent(nodes.arrangeSlot, canvas && toolbar.arrange !== null, movers, {
    focusTarget: nodes.networkChip,
  });
  setSlotPresent(
    nodes.pauseSlot,
    canvas && toolbar.pause !== null,
    [...movers, nodes.arrangeSlot],
    {
      focusTarget: nodes.networkChip,
    },
  );
  renderNetworkChip(ctx);
  nodes.studio.dataset.arranging = String(studio.arrange.active);
}

function eligibleNetworks() {
  return (state.snapshot?.interfaces ?? []).filter(isSelectable);
}

// The chip names the network MonHop uses; its menu lists every network it could use instead.
function renderNetworkChip(ctx) {
  const selected = selectedInterface(state);
  const eligible = eligibleNetworks();
  const chip = networkChip({
    selected,
    label: selected ? networkLabel(selected) : "",
    eligibleCount: eligible.length,
    sessionActive: isSessionActive(sharing),
  });
  const width = nodes.networkChip.getBoundingClientRect().width;
  setLabel(nodes.networkChipLabel, chip.text);
  if (width) glideWidth(nodes.networkChip, width);
  nodes.networkChip.title = chip.hint;
  nodes.networkChip.disabled = chip.disabled || ctx.busy;
  nodes.networkChipDot.dataset.tone = selected ? "connected" : "neutral";
  if (nodes.networkChip.disabled && networkMenuOpen) setNetworkMenuOpen(false, { render: false });
  clear(nodes.networkMenu);
  nodes.networkMenu.append(
    el("p", { className: "menu-heading", text: "Network" }),
    ...eligible.map((item) => {
      const checked = state.selectedInterfaceId === item.id;
      const option = el("button", {
        className: "menu-item",
        attrs: { type: "button", role: "menuitemradio", "aria-checked": String(checked) },
        dataset: { focusKey: `network-menu-${item.id}` },
        children: [
          el("span", { className: "menu-check", children: [checked ? icon("check", 13) : null] }),
          el("span", {
            className: "menu-item-copy",
            children: [
              el("strong", { text: networkLabel(item) }),
              el("span", { text: item.address || item.name }),
            ],
          }),
        ],
      });
      option.addEventListener("click", () => {
        setNetworkMenuOpen(false);
        nodes.networkChip.focus({ preventScroll: true });
        chooseInterface(item.id);
      });
      return option;
    }),
    el("p", { className: "menu-note", text: chip.hint }),
  );
}

function setNetworkMenuOpen(open, { render: draw = true } = {}) {
  const next = open && !nodes.networkChip.disabled;
  if (next === networkMenuOpen) return;
  networkMenuOpen = next;
  if (!next) handOffFocus(nodes.networkMenu, nodes.networkChip);
  nodes.networkMenu.dataset.open = String(next);
  nodes.networkMenu.inert = !next;
  nodes.networkChip.setAttribute("aria-expanded", String(next));
  if (next && draw) nodes.networkMenu.querySelector("[aria-checked='true'], .menu-item")?.focus();
}

// ---------- arranging and pausing ----------

// Arrange opens the editor with the computer on the canvas; the canvas waits for the link.
function arrangeWith(fingerprint) {
  const target =
    fingerprint ??
    canvasFingerprint({
      selection: currentSelection(),
      active: activeFingerprint(),
      fingerprints: computers.items.map((item) => item.fingerprint),
    });
  if (!target || !canEditLayout(sharing, state.selectedInterfaceId)) return;
  if (currentSelection().kind === "settings") chosen = computerSelection(target);
  arrangeRequested = true;
  canvasDrawer = null;
  render();
  void beginLayoutEdit(target);
}

// Done and Cancel both end the edit; what was applied stays applied.
function leaveArrange() {
  arrangeRequested = false;
  canvasDrawer = null;
  render();
  void endLayoutEdit();
}

function toggleCanvasDrawer(kind) {
  canvasDrawer = canvasDrawer === kind ? null : kind;
  render();
}

// The same as the tray's Pause sharing: every computer off, input stays here.
function pauseSharing() {
  lastActive = activeFingerprint() ?? lastActive;
  void useComputer(null);
}

function resumeSharing() {
  const target = resumeTarget({
    lastActive,
    selection: currentSelection(),
    fingerprints: computers.items.map((item) => item.fingerprint),
  });
  if (target) void setComputerEnabled(target, true);
}

// ---------- snapshot / access / network ----------

async function refreshSnapshot() {
  await takeSnapshot({ automatic: false, trigger: "manual" });
}

async function requestAutomaticSnapshot(trigger) {
  if (
    !shouldStartAutomaticSnapshot({
      uiCheck,
      nativeAvailable: state.nativeAvailable,
      busy: controlsBusy(),
      checking: snapshotCheck.pending,
      automaticFailed: snapshotCheck.automaticFailed,
      trigger,
    })
  )
    return;
  await takeSnapshot({ automatic: true, trigger });
}

async function takeSnapshot({ automatic, trigger }) {
  if (!core?.invoke || snapshotCheck.pending || controlsBusy()) return;
  snapshotCheck = {
    pending: true,
    automaticFailed: automatic ? snapshotCheck.automaticFailed : false,
    freshness: "Checking this computer…",
  };
  state = { ...state, busy: true, actionFailed: false };
  render();
  try {
    const snapshot = await core.invoke("setup_snapshot");
    applySnapshotCheck({ ...state, messages: [] }, snapshot, snapshotFreshness(trigger));
  } catch (error) {
    snapshotCheck = {
      pending: false,
      automaticFailed: true,
      freshness: "Could not check this computer. Use refresh to try again.",
    };
    state = { ...state, actionFailed: true, snapshot: null, messages: [nativeError(error)] };
  } finally {
    state = { ...state, busy: false };
    render();
    if (showsReadiness(currentSelection())) openPairingOnExplicitEntry();
  }
}

function snapshotFreshness(trigger) {
  if (trigger === "startup") return "Checked on launch.";
  if (trigger === "focus") return "Checked after returning to MonHop.";
  if (trigger === "entry") return "Checked as setup opened.";
  return "Checked just now.";
}

async function requestPermissions() {
  await runNative("request_permissions", undefined, (snapshot) =>
    applySnapshotCheck({ ...state, messages: [] }, snapshot, "Checked after requesting access."),
  );
}

async function requestWifiPermission() {
  await runNative("request_wifi_permission", undefined, async () => {
    const snapshot = await core.invoke("setup_snapshot");
    applySnapshotCheck(
      {
        ...state,
        messages: [
          "macOS may show a Location Services prompt. Complete it, then return to MonHop.",
        ],
      },
      snapshot,
      "Checked after requesting Location access.",
    );
  });
}

function applySnapshotCheck(nextState, snapshot, freshness) {
  state = autoSelectInterface(applySnapshot(nextState, snapshot), computers.interfaceId);
  snapshotCheck = { pending: false, ...snapshotCheckResult(state.snapshot, freshness) };
}

// A new network reopens pairing on it, so a code is only ever shown or entered on the network the
// user selected; a finished snapshot does the same from takeSnapshot.
function chooseInterface(id) {
  if (controlsBusy() || isSessionActive(sharing)) return;
  state = selectInterface(state, id);
  render();
  if (showsReadiness(currentSelection())) openPairingOnExplicitEntry();
}

async function openSettings(pane) {
  await runNative("open_permission_settings", { pane }, () => {
    state = {
      ...state,
      messages: [
        pane === "local-network"
          ? "In System Settings, open Privacy & Security, then Local Network, and allow MonHop. Then pair again."
          : pane === "location-services"
            ? "Allow Location Services for MonHop in System Settings, then return here."
            : "Enable MonHop in the Settings pane that opened, then return here. MonHop checks again automatically.",
      ],
    };
  });
}

async function runNative(command, payload, onSuccess) {
  if (!core?.invoke) {
    state = { ...state, messages: ["Open MonHop to run this action. The browser preview cannot."] };
    render();
    return;
  }
  state = { ...state, busy: true, actionFailed: false };
  render();
  try {
    const result =
      payload === undefined ? await core.invoke(command) : await core.invoke(command, payload);
    await onSuccess(result);
  } catch (error) {
    state = {
      ...state,
      actionFailed: true,
      snapshot: ["setup_snapshot", "request_permissions", "request_wifi_permission"].includes(
        command,
      )
        ? null
        : state.snapshot,
      messages: [nativeError(error)],
    };
  } finally {
    state = { ...state, busy: false };
    render();
  }
}

// ---------- pairing ----------

function pairingEntryContext() {
  const selected = selectedNetworkContextKey(selectedInterface(state));
  return selected ? JSON.stringify([selected, state.snapshot?.platform]) : "";
}

function openPairingOnExplicitEntry() {
  const contextKey = pairingEntryContext();
  if (
    !canOpenPairingOnEntry({
      uiCheck,
      nativeAvailable: state.nativeAvailable,
      busy: controlsBusy(),
      eligible: pairingOpenGate(state).allowed,
      contextKey,
      attemptedKeys: autoPairingContexts,
    })
  )
    return;
  void openPairing();
}

async function openPairing() {
  const contextKey = pairingEntryContext();
  if (!pairingOpenGate(state).allowed || !contextKey) return;
  autoPairingContexts = recordPairingOpenContext(autoPairingContexts, contextKey);
  pairing = withPairingMode(pairing, null);
  await invokePairing("pairing_open", { interfaceId: state.selectedInterfaceId });
}

// The exchange opens on the step that picks a role, reading the identity first when it is closed.
function enterPairing() {
  pairingResultShown = false;
  if (!pairing.view || pairing.view.phase === "closed")
    return pairingOpenGate(state).allowed ? openPairing() : null;
  if (!isBusyPairing(pairing)) pairing = withPairingMode(pairing, "choose");
  return null;
}

// Pair another computer: back to the New computer row and a fresh exchange.
function beginPairing() {
  if (currentSelection().kind === "new") {
    void enterPairing();
    render();
    return;
  }
  void select("new");
}

// Show a code or Enter a code from This Mac's inspector, which first selects the New computer row.
async function startPairing(mode) {
  const opening = select("new");
  if (opening) await opening;
  if (currentSelection().kind !== "new") return;
  if (mode === "show") await showPairingCode();
  else choosePairingMode("enter");
}

// Close or Done: the exchange gives way to the computer it added, or to the default row.
function dismissPairing() {
  const paired = justPaired();
  leavePairing();
  if (paired) chosen = computerSelection(paired);
  else if (chosen?.kind === "new") chosen = null;
  render();
}

async function createPairingIdentity() {
  if (!pairingOpenGate(state).allowed) return;
  await invokePairing("pairing_create_identity", { interfaceId: state.selectedInterfaceId });
}

// Picking a way in ("choose" or "enter") is local until the user shows or submits a code.
function choosePairingMode(mode) {
  pairing = withPairingMode(pairing, mode);
  render();
  if (mode === "enter") document.getElementById(PAIRING_CODE_INPUT_ID)?.focus();
}

function editPairingCode(value) {
  pairing = editPairingEntry(pairing, value);
  render();
}

// Also the New code action: showing again burns the code on screen and shows a fresh one.
async function showPairingCode() {
  if (pairingPending || !(canStartPairing(pairing) || pairing.view?.phase === "showing")) return;
  pairingPollingAllowed = true;
  await invokePairing("pairing_show_code");
}

async function submitPairingCode() {
  if (pairingPending || !canSubmitPairingCode(pairing)) return;
  pairingPollingAllowed = true;
  await invokePairing("pairing_enter_code", { code: pairing.entry });
}

// Cancelling a running attempt unwinds through `stopping`, which the poll follows back to ready.
async function cancelPairing() {
  pairing = withPairingMode(pairing, null);
  pairingPollingAllowed = true;
  await invokePairing("pairing_cancel");
}

async function invokePairing(command, payload) {
  if (!core?.invoke) {
    pairing = pairingFailure(pairing, "Open MonHop to pair computers. The browser preview cannot.");
    render();
    return;
  }
  const generation = ++pairingGeneration;
  stopPairingPoll();
  pairingPending = true;
  pairingOperation = command;
  pairing = { ...pairing, message: "" };
  render();
  try {
    const result =
      payload === undefined ? await core.invoke(command) : await core.invoke(command, payload);
    if (generation !== pairingGeneration) return;
    applyPairingResult(result);
    if (pairingPollingAllowed && isBusyPairing(pairing)) schedulePairingPoll();
    else if (!isBusyPairing(pairing)) pairingPollingAllowed = false;
  } catch (error) {
    if (generation !== pairingGeneration) return;
    // A typo or an address off this network is caught before anything is sent; it belongs under
    // the field, which keeps what was typed.
    pairing =
      command === "pairing_enter_code"
        ? failPairingEntry(pairing, nativeError(error))
        : pairingFailure(pairing, nativeError(error));
    pairingPollingAllowed = false;
  } finally {
    if (generation === pairingGeneration) {
      pairingPending = false;
      pairingOperation = null;
      render();
      if (command === "pairing_enter_code" && pairing.entryError)
        document.getElementById(PAIRING_CODE_INPUT_ID)?.focus();
    }
  }
}

function schedulePairingPoll() {
  if (!pairingPollingAllowed || pairingPollTimer || !isBusyPairing(pairing)) return;
  pairingPollTimer = window.setTimeout(async () => {
    pairingPollTimer = null;
    const generation = pairingGeneration;
    try {
      const result = await core.invoke("pairing_status");
      if (generation !== pairingGeneration) return;
      applyPairingResult(result);
      render();
      if (isBusyPairing(pairing)) schedulePairingPoll();
      else pairingPollingAllowed = false;
    } catch (error) {
      if (generation !== pairingGeneration) return;
      pairing = pairingFailure(
        pairing,
        `Could not update the pairing status: ${nativeError(error)}`,
      );
      pairingPollingAllowed = false;
      render();
    }
  }, PAIRING_POLL_MS);
}

function stopPairingPoll() {
  if (pairingPollTimer !== null) window.clearTimeout(pairingPollTimer);
  pairingPollTimer = null;
}

// Between status polls the countdown text ticks on its own, and the code leaves the screen the
// moment it expires.
function syncPairingCountdown() {
  const ticking = pairing.view?.phase === "showing" && codeTimeLeft(pairing.view) > 0;
  if (!ticking) {
    if (pairingCountdownTimer !== null) window.clearInterval(pairingCountdownTimer);
    pairingCountdownTimer = null;
    return;
  }
  if (pairingCountdownTimer !== null) return;
  pairingCountdownTimer = window.setInterval(() => {
    const left = codeTimeLeft(pairing.view);
    if (!left) {
      window.clearInterval(pairingCountdownTimer);
      pairingCountdownTimer = null;
      render();
      return;
    }
    const text = document.getElementById(COUNTDOWN_TEXT_ID);
    if (text) text.textContent = countdownText(left);
  }, PAIRING_COUNTDOWN_TICK_MS);
}

function applyPairingResult(result) {
  const current = pairing;
  pairing = applyPairingView(current, result);
  // One exchange ends once: the new computer joins the list and the supervisor connects to it.
  // Leaving `paired` re-arms it, so pairing the same computer again is still a new exchange.
  const key = pairing.view?.phase === "paired" ? pairing.view.peerFingerprint : null;
  if (!key) {
    pairedKey = null;
    return;
  }
  if (key === pairedKey) return;
  pairedKey = key;
  // Only an attempt finishing here shows its badge; a paired state read back on open does not.
  pairingResultShown = isBusyPairing(current);
  // The new computer's row takes over from the exchange once the list names it.
  if (pairingResultShown && currentSelection().kind === "new") chosen = computerSelection(key);
  void loadComputers();
  void refreshSharingStatus();
}

// ---------- computers ----------

async function loadComputers() {
  if (!core?.invoke) return;
  const requested = requestComputersLoad(computersLoad);
  computersLoad = requested.load;
  if (requested.start) await readComputerList();
}

// One read, then the one more that any request made meanwhile asked for.
async function readComputerList() {
  const request = ++computersRequest;
  render();
  try {
    // Read before the list, so the list is at least as new as this revision.
    const status = await core.invoke("sharing_status").catch(() => null);
    computersLoad = coverComputersLoad(computersLoad, normalizeSharingView(status).setupRevision);
    applyComputers(await readComputers("computers_load"), request);
    if (request === computersRequest) refreshComputerArrangements();
  } catch {
    // Not covered after all, so the next status poll reads again.
    computersLoad = coverComputersLoad(computersLoad, null);
    if (request === computersRequest)
      computersLoadFailure = "The paired computers could not be read.";
  }
  const finished = finishComputersLoad(computersLoad);
  computersLoad = finished.load;
  if (request === computersRequest) render();
  if (finished.again) await readComputerList();
}

// Called with every sharing view applied: a commit, or a display change on either computer.
function followComputers() {
  const followed = followSetupRevision(computersLoad, sharing.view?.setupRevision);
  computersLoad = followed.load;
  if (followed.start) void readComputerList();
}

async function readComputers(command, payload) {
  const next = normalizeComputers(
    payload === undefined ? await core.invoke(command) : await core.invoke(command, payload),
  );
  if (!next.loaded) throw new Error(LIST_UNREADABLE);
  return next;
}

function applyComputers(next, request) {
  if (request !== computersRequest) return;
  computers = next;
  computersLoadFailure = "";
  if (!state.selectedInterfaceId && state.snapshot)
    state = autoSelectInterface(state, next.interfaceId);
  if (renaming && !findComputer(computers, renaming)) renaming = renameDraft = null;
  if (forgetConfirmed && !findComputer(computers, forgetConfirmed)) forgetConfirmed = null;
  layoutForget = keepForgetArmed(
    layoutForget,
    computers.items.map((item) => item.fingerprint),
  );
  // A forgotten computer's drawn positions must not seed a slide if it is ever paired again.
  const kept = new Set(computers.items.map((item) => item.fingerprint));
  for (const fingerprint of Object.keys(computerArrangementsStore))
    if (!kept.has(fingerprint)) forgetArrangementMotion(fingerprint);
  computerArrangementsStore = pruneComputerArrangements(computerArrangementsStore, computers);
  scheduleSharingPoll();
}

function startRename(fingerprint) {
  renaming = fingerprint;
  renameDraft = findComputer(computers, fingerprint)?.name ?? "";
  render();
  const field = document.getElementById(`rename-field-${fingerprint}`);
  field?.focus({ preventScroll: true });
  field?.select();
}

function draftRename(name) {
  renameDraft = name;
}

function cancelRename() {
  renaming = renameDraft = null;
  render();
}

async function renameComputer(fingerprint, name) {
  const trimmed = name.trim();
  if (!core?.invoke || !trimmed || trimmed.length > 48 || renamePending) return;
  renamePending = fingerprint;
  const request = ++computersRequest;
  render();
  try {
    applyComputers(
      await readComputers("computers_rename", { fingerprint, name: trimmed }),
      request,
    );
    renaming = renameDraft = null;
  } catch (error) {
    if (request === computersRequest)
      state = { ...state, messages: [`Could not rename the computer: ${nativeError(error)}`] };
  } finally {
    if (renamePending === fingerprint) renamePending = null;
    if (request === computersRequest) render();
  }
}

function confirmForget(fingerprint, checked) {
  forgetConfirmed = checked === true ? fingerprint : null;
  render();
}

async function forgetComputer(fingerprint) {
  if (!core?.invoke || forgetConfirmed !== fingerprint || controlsBusy()) return;
  forgetConfirmed = null;
  const request = ++computersRequest;
  state = { ...state, busy: true };
  render();
  try {
    applyComputers(await readComputers("pairing_forget", { fingerprint }), request);
  } catch (error) {
    state = { ...state, messages: [`Could not forget the computer: ${nativeError(error)}`] };
  } finally {
    state = { ...state, busy: false };
    render();
  }
  await refreshSharingStatus();
}

// ---------- the connection ----------

async function useComputer(fingerprint) {
  const interfaceId = state.selectedInterfaceId ?? null;
  await runSharing("active", () => core.invoke("sharing_set_active", { fingerprint, interfaceId }));
}

// Switches one computer on or off within the group everything else is enabled. Falls back to
// today's single-active-computer command on its own (see invokeSetComputerEnabled) so this keeps
// working unchanged against a backend built before sharing_set_enabled existed.
async function setComputerEnabled(fingerprint, enabled) {
  const interfaceId = state.selectedInterfaceId ?? null;
  await runSharing("enabled", () =>
    invokeSetComputerEnabled(
      (command, payload) => core.invoke(command, payload),
      fingerprint,
      enabled,
      interfaceId,
    ),
  );
}

async function beginLayoutEdit(fingerprint = activeFingerprint()) {
  const interfaceId = state.selectedInterfaceId;
  if (!fingerprint || !canEditLayout(sharing, interfaceId)) return;
  await runSharing("edit", () => core.invoke("sharing_edit_begin", { interfaceId, fingerprint }));
}

async function endLayoutEdit() {
  if (!isEditingLayout(sharing)) return;
  await runSharing("edit-end", () => core.invoke("sharing_edit_end"));
}

// A display notice's way in: arranging with the computer in use.
function changeLayout() {
  arrangeWith(activeFingerprint());
}

async function dismissDisplayNotice() {
  if (!sharing.view?.displayNotice) return;
  await runSharing("dismiss-notice", () => core.invoke("sharing_dismiss_display_notice"));
}

// Either direction can be turned off independently; the backend refuses turning off the last one.
async function setControl(fingerprint, direction, allowed) {
  if (!core?.invoke) return;
  await runSharing("control", () =>
    core.invoke("sharing_set_control", { fingerprint, direction, allowed }),
  );
}

async function applySetup() {
  const layout = layoutForSave(sharing);
  if (!core?.invoke || !layout || !canApplySetup(sharing)) return;
  await runSharing("apply", () =>
    core.invoke("sharing_apply_setup", { revision: sharing.view.revision, layout }),
  );
}

// A connected link always has a draft to edit, so every view settles into one.
// A new drop invalidates whatever copy feedback the previous one left behind.
function applySharing(current, view) {
  if (current.view?.lastFailure !== view?.lastFailure) {
    dropCopyRequest += 1;
    dropCopyFeedback = emptyCopyFeedback();
  }
  return initializeArrangement(applySharingView(current, view));
}

async function copyLastDrop() {
  const text = sharing.view?.lastFailure;
  if (!text || dropCopyFeedback.state === "pending") return;
  const request = ++dropCopyRequest;
  const isCurrent = () =>
    isCopyReplyCurrent(request, dropCopyRequest, text, sharing.view?.lastFailure);
  if (!core?.invoke) {
    dropCopyFeedback = copyFeedbackFailure(text, request, "Open MonHop to copy this.");
    render();
    return;
  }
  dropCopyFeedback = beginCopyFeedback(text, request);
  render();
  try {
    await core.invoke("sharing_copy_last_drop");
    if (!isCurrent()) return;
    dropCopyFeedback = copyFeedbackSuccess(text, request, "Copied.");
    window.setTimeout(() => {
      if (isCurrent()) {
        dropCopyFeedback = emptyCopyFeedback();
        render();
      }
    }, 1500);
  } catch (error) {
    if (!isCurrent()) return;
    dropCopyFeedback = copyFeedbackFailure(text, request, `Could not copy: ${nativeError(error)}`);
  }
  render();
}

async function loadDimming() {
  await runDimming(() => core.invoke("dimming_status"));
}

// A shortcut press changes the overlay outside this window; Rust announces every change.
function listenDimming() {
  const listen = window.__TAURI__?.event?.listen;
  if (typeof listen !== "function") return;
  void listen("dimming", (event) => {
    dimming = applyDimmingView(dimming, event.payload);
    render();
  });
}

// One dimming request at a time; a reply that predates a newer one is dropped.
async function runDimming(invoke) {
  if (!core?.invoke || dimming.pending) return;
  const request = ++dimmingRequest;
  dimming = beginDimming(dimming);
  render();
  try {
    const view = await invoke();
    if (request !== dimmingRequest) return;
    dimming = applyDimmingView(dimming, view);
  } catch (error) {
    if (request !== dimmingRequest) return;
    dimming = failDimming(dimming, nativeError(error));
  }
  render();
}

function setDimmingEnabled(enabled) {
  void runDimming(() => core.invoke("dimming_set_enabled", { enabled }));
}

function setDimLevel(level) {
  const clamped = clampLevel(dimming.view, level);
  void runDimming(() => core.invoke("dimming_set_level", { level: clamped }));
}

// Drag steps go straight to the overlay, one per frame; the view changes on the settled value.
function previewDimLevel(level) {
  const clamped = clampLevel(dimming.view, level);
  if (!core?.invoke) return;
  if (dimPreview) {
    dimPreview.level = clamped;
    return;
  }
  dimPreview = { level: clamped };
  window.requestAnimationFrame(() => {
    const { level: settled } = dimPreview;
    dimPreview = null;
    core.invoke("dimming_preview_level", { level: settled }).catch(() => {});
  });
}

function toggleDimming() {
  void runDimming(() => core.invoke("dimming_toggle"));
}

function beginDimDrag() {
  dimmingDrag = true;
}

function endDimDrag() {
  dimmingDrag = false;
  if (!renderAfterDrag) return;
  renderAfterDrag = false;
  render();
}

// ---------- clipboard ----------

async function loadClipboard() {
  if (!core?.invoke) return;
  try {
    clipboardView = await core.invoke("clipboard_status");
    render();
  } catch {
    // No such command on this backend yet (or a transient failure): the card stays hidden until a
    // status read succeeds or a "clipboard" event proves the backend actually has it.
  }
}

// Rust announces every change (a toggle here, on the other computer, or a peer attaching), so the
// card stays live without polling.
function listenClipboard() {
  const listen = window.__TAURI__?.event?.listen;
  if (typeof listen !== "function") return;
  void listen("clipboard", (event) => {
    clipboardView = event.payload;
    render();
  });
}

// One in-flight clipboard command at a time; a failure surfaces in the page alert like updates and
// autostart do, since the card itself carries no failure text of its own.
async function runClipboardCommand(invoke) {
  if (!core?.invoke || clipboardPending) return;
  clipboardPending = true;
  render();
  try {
    clipboardView = await invoke();
  } catch (error) {
    state = { ...state, messages: [nativeError(error)] };
  } finally {
    clipboardPending = false;
    render();
  }
}

function setClipboardEnabled(enabled) {
  void runClipboardCommand(() => core.invoke("clipboard_set_enabled", { enabled }));
}

// ---------- mouse and trackpad ----------

// Both answers land in one render, so the card enters whole instead of growing a row. A backend
// without a command leaves that row out.
async function loadPointerSwitches() {
  if (!core?.invoke) return;
  const status = (command) => core.invoke(command).catch(() => undefined);
  const [autoscroll, swipe] = await Promise.all([
    status("autoscroll_status"),
    status("swipe_status"),
  ]);
  pointerSwitches.autoscroll.view = autoscroll;
  pointerSwitches.swipe.view = swipe;
  render();
}

// One in-flight change per switch; a failed call surfaces in the page alert, a failed save on the card.
async function setSavedSwitch(entry, command, enabled) {
  if (!core?.invoke || entry.pending) return;
  entry.pending = true;
  render();
  try {
    entry.view = await core.invoke(command, { enabled });
  } catch (error) {
    state = { ...state, messages: [nativeError(error)] };
  } finally {
    entry.pending = false;
    render();
  }
}

function setAutoscrollEnabled(enabled) {
  void setSavedSwitch(pointerSwitches.autoscroll, "autoscroll_set_enabled", enabled);
}

function setSwipeEnabled(enabled) {
  void setSavedSwitch(pointerSwitches.swipe, "swipe_set_enabled", enabled);
}

// ---------- keyboard ----------

async function loadControlAsCommand() {
  if (!core?.invoke) return;
  // A backend without the command leaves the card out.
  controlAsCommand.view = await core.invoke("control_as_command_status").catch(() => undefined);
  render();
}

function setControlAsCommand(enabled) {
  void setSavedSwitch(controlAsCommand, "control_as_command_set_enabled", enabled);
}

// ---------- updates ----------

async function loadUpdatesStatus() {
  if (!core?.invoke) return;
  try {
    updates = normalizeUpdatesView(await core.invoke("updates_status"));
    render();
  } catch {
    // A stale card is safer than one that vanished; the next entry or event retries.
  }
}

// Rust announces every phase change, so the card stays live without polling.
function listenUpdates() {
  const listen = window.__TAURI__?.event?.listen;
  if (typeof listen !== "function") return;
  void listen("updates", (event) => {
    updates = normalizeUpdatesView(event.payload);
    render();
  });
}

// One in-flight updates command at a time; the pending flag alone guards re-entry.
async function runUpdatesCommand(invoke) {
  if (!core?.invoke || updatesPending) return;
  updatesPending = true;
  render();
  try {
    updates = normalizeUpdatesView(await invoke());
  } catch (error) {
    state = { ...state, messages: [nativeError(error)] };
  } finally {
    updatesPending = false;
    render();
  }
}

function setUpdatesAutomatic(enabled) {
  void runUpdatesCommand(() => core.invoke("updates_set_automatic", { enabled }));
}

function checkForUpdates() {
  if (!canCheck(updates)) return;
  void runUpdatesCommand(() => core.invoke("updates_check"));
}

function installUpdate() {
  if (!canInstall(updates)) return;
  void runUpdatesCommand(() => core.invoke("updates_install"));
}

// ---------- autostart ----------

async function loadAutostartStatus() {
  if (!core?.invoke) return;
  try {
    autostart = normalizeAutostartView(await core.invoke("autostart_status"));
    render();
  } catch {
    // A stale switch is safer than one that vanished; the next page entry retries.
  }
}

// One in-flight autostart command at a time; the pending flag alone guards re-entry.
async function runAutostartCommand(invoke) {
  if (!core?.invoke || autostartPending) return;
  autostartPending = true;
  render();
  try {
    await invoke();
  } catch (error) {
    state = { ...state, messages: [nativeError(error)] };
  } finally {
    autostartPending = false;
    render();
  }
}

function setAutostart(enabled) {
  void runAutostartCommand(async () => {
    autostart = normalizeAutostartView(await core.invoke("autostart_set", { enabled }));
  });
}

function openAutostartSettings() {
  void runAutostartCommand(() => core.invoke("autostart_open_settings"));
}

// ---------- notch island ----------

async function loadIslandStatus() {
  if (!core?.invoke || islandPending) return;
  try {
    island = normalizeIslandView(await core.invoke("island_status"));
    render();
  } catch {
    // The card keeps what it last showed; the next Settings visit asks again.
  }
}

async function setIslandEnabled(enabled) {
  if (!core?.invoke || islandPending) return;
  islandPending = true;
  render();
  try {
    island = normalizeIslandView(await core.invoke("island_set_enabled", { enabled }));
  } catch (error) {
    state = { ...state, messages: [nativeError(error)] };
  } finally {
    islandPending = false;
    render();
  }
}

async function openLink(target) {
  if (!core?.invoke) return;
  try {
    await core.invoke("app_open_link", { target });
  } catch (error) {
    state = { ...state, messages: [nativeError(error)] };
    render();
  }
}

// One in-flight connection command at a time; a reply that predates a newer one is dropped.
async function runSharing(kind, invoke) {
  if (!core?.invoke || isActionBusy(sharing)) return;
  sharing = beginPending(sharing, kind);
  const generation = sharing.generation;
  render();
  try {
    const view = await invoke();
    if (!isCurrentPending(sharing, generation)) return;
    sharing = applySharing(settlePending(sharing, generation), view);
  } catch (error) {
    sharing = failPending(sharing, generation, nativeError(error));
  }
  render();
  followComputers();
  scheduleSharingPoll();
}

function commitArrangement(placement, moving) {
  if (controlsBusy() || !isConnected(sharing)) return;
  sharing = setArrangement(sharing, placement, moving);
  touchSetupLink();
  render();
}

function showMonitorOn(monitor, side) {
  if (controlsBusy() || !isConnected(sharing)) return;
  sharing = setMonitorSide(sharing, monitor, side);
  touchSetupLink();
  render();
}

function useDisplay(id, inUse) {
  if (controlsBusy() || !isConnected(sharing)) return;
  sharing = setDisplayInUse(sharing, id, inUse);
  touchSetupLink();
  render();
}

function resetArrangement() {
  if (controlsBusy() || !isConnected(sharing)) return;
  sharing = resetSharingArrangement(sharing);
  touchSetupLink();
  render();
}

// Arranging keeps the setup link's idle window open for as long as the user is working.
function touchSetupLink() {
  if (core?.invoke && isLinkActive(sharing)) void core.invoke("sharing_touch").catch(() => {});
}

async function refreshSharingStatus() {
  if (!core?.invoke || sharingStatusPending) return;
  sharingStatusPending = true;
  const generation = sharing.generation;
  // A render that only rebuilds the same view is wasted work, so a poll that changes nothing skips it.
  let changed = true;
  try {
    const view = await core.invoke("sharing_status");
    // A reply that predates a newer command is stale, whatever it says.
    if (sharing.pending === null && generation === sharing.generation) {
      const next = applySharing(sharing, view);
      changed = !sameSharingView(next.view, sharing.view);
      sharing = next;
    }
  } catch (error) {
    sharing = {
      ...sharing,
      message: `Could not read the connection status: ${nativeError(error)}`,
    };
  } finally {
    sharingStatusPending = false;
  }
  if (changed) render();
  followComputers();
  scheduleSharingPoll();
}

function scheduleSharingPoll() {
  if (
    sharingPollTimer !== null ||
    !core?.invoke ||
    !shouldPollSharing(sharing, pollFingerprint(computers))
  )
    return;
  sharingPollTimer = window.setTimeout(() => {
    sharingPollTimer = null;
    void refreshSharingStatus();
  }, SHARING_POLL_MS);
}

// ---------- saved arrangements ----------

// The library lists only what fits the connected pair, so the list follows the link's topology revision.
function syncArrangements() {
  if (!core?.invoke) return;
  // Only a connected link has the inspection the library is filtered by; a dialing link would list nothing.
  const revision = sharing.view?.phase === "connected" ? sharing.view.revision : null;
  if (revision === arrangementsRevision || Date.now() < arrangementsRetry.at) return;
  arrangementsRevision = revision;
  if (revision !== null) void loadArrangements(revision);
}

// A reply belongs to the revision it was asked for; a failed read is retried on later polls, backing off.
async function loadArrangements(revision) {
  try {
    const list = await core.invoke("sharing_arrangements");
    if (sharing.view?.revision !== revision || !isLinkActive(sharing)) return;
    sharing = setArrangements(sharing, list);
    arrangementsRetry = { at: 0, delay: 0 };
  } catch (error) {
    if (arrangementsRevision === revision) arrangementsRevision = null;
    arrangementsRetry = {
      at: Date.now() + arrangementsRetry.delay,
      delay: Math.min(Math.max(arrangementsRetry.delay * 2, 2000), 30000),
    };
    sharing = {
      ...sharing,
      message: `Could not read the saved arrangements: ${nativeError(error)}`,
    };
  }
  render();
}

async function saveArrangement(name) {
  const layout = layoutForSave(sharing);
  if (!core?.invoke || !layout || controlsBusy() || !canSaveArrangement(sharing, name)) return;
  await runArrangementCommand("sharing_arrangement_save", {
    revision: sharing.view.revision,
    name,
    layout,
  });
}

async function deleteArrangement(name) {
  const fingerprint = activeFingerprint();
  if (!core?.invoke || !fingerprint || controlsBusy() || !arrangementByName(sharing, name)) return;
  await runArrangementCommand("sharing_arrangement_forget", { fingerprint, name });
}

// Both library commands answer with the whole list, so one path applies either reply: to the
// editor's list and to the computer card's history, which show the same library.
async function runArrangementCommand(command, payload) {
  const fingerprint = activeFingerprint();
  sharing = beginPending(sharing, "arrangement");
  const generation = sharing.generation;
  const request = (arrangementRequests.get(fingerprint) ?? 0) + 1;
  if (fingerprint) arrangementRequests.set(fingerprint, request);
  layoutForget = clearForgetFor(layoutForget, fingerprint);
  render();
  try {
    const list = await core.invoke(command, payload);
    if (!isCurrentPending(sharing, generation)) return;
    sharing = setArrangements(settlePending(sharing, generation), list);
    if (fingerprint && arrangementRequests.get(fingerprint) === request)
      computerArrangementsStore = setComputerArrangements(
        computerArrangementsStore,
        fingerprint,
        list,
      );
  } catch (error) {
    sharing = failPending(sharing, generation, nativeError(error));
  }
  render();
}

function loadSavedArrangement(name) {
  if (controlsBusy() || !canLoadArrangement(sharing, name)) return;
  sharing = loadArrangement(sharing, name);
  touchSetupLink();
  render();
}

// ---------- each computer's layout history ----------

// Read on demand (the disclosure opening, or after a forget) rather than polled, since it works
// disconnected and most cards never open it. Once read, it follows every read of the computers.
async function loadComputerArrangements(fingerprint) {
  if (
    !core?.invoke ||
    !fingerprint ||
    computerArrangements(computerArrangementsStore, fingerprint).loading
  )
    return;
  await readArrangementsFor(fingerprint, listArrangementsFor(fingerprint));
}

function listArrangementsFor(fingerprint) {
  return () => core.invoke("sharing_arrangements_for", { fingerprint });
}

// A commit remembers its layout beside the setup it writes, so every list already read is read
// again with the computers. One with its own read or forget in flight is read after that ends.
function refreshComputerArrangements() {
  for (const fingerprint of Object.keys(computerArrangementsStore)) {
    if (computerArrangements(computerArrangementsStore, fingerprint).loading)
      arrangementsRefreshAfter.add(fingerprint);
    else void readArrangementsFor(fingerprint, listArrangementsFor(fingerprint), { quiet: true });
  }
}

// Forget is armed on the first press and runs on the second, one row at a time across the app.
function pressLayoutForget(fingerprint, name) {
  const { armed, forget } = pressForget(layoutForget, fingerprint, name);
  layoutForget = armed;
  // Drawn first either way: a forget the app is too busy to run must not leave the row armed.
  render();
  if (forget) void forgetComputerArrangement(fingerprint, name);
}

async function forgetComputerArrangement(fingerprint, name) {
  if (
    !core?.invoke ||
    !fingerprint ||
    controlsBusy() ||
    computerArrangements(computerArrangementsStore, fingerprint).loading
  )
    return;
  await readArrangementsFor(fingerprint, () =>
    core.invoke("sharing_arrangement_forget", { fingerprint, name }),
  );
}

// One command per computer at a time. Its reply is the whole list, so it lands in that computer's
// history and, when this is the computer in use, in the connected editor's list as well. A quiet
// read leaves the rows as they are while it runs.
async function readArrangementsFor(fingerprint, invoke, { quiet = false } = {}) {
  const request = (arrangementRequests.get(fingerprint) ?? 0) + 1;
  arrangementRequests.set(fingerprint, request);
  if (!quiet) {
    // The list is about to be replaced, so a confirm armed against the old one is dropped.
    layoutForget = clearForgetFor(layoutForget, fingerprint);
    computerArrangementsStore = beginComputerArrangements(computerArrangementsStore, fingerprint);
    render();
  }
  try {
    const list = await invoke();
    if (arrangementRequests.get(fingerprint) !== request) return;
    applyArrangementList(fingerprint, list);
  } catch (error) {
    if (arrangementRequests.get(fingerprint) !== request) return;
    computerArrangementsStore = failComputerArrangements(
      computerArrangementsStore,
      fingerprint,
      nativeError(error),
    );
  }
  layoutForget = keepForgetListed(
    layoutForget,
    fingerprint,
    computerArrangements(computerArrangementsStore, fingerprint).items,
  );
  render();
  if (arrangementsRefreshAfter.delete(fingerprint))
    void readArrangementsFor(fingerprint, listArrangementsFor(fingerprint), { quiet: true });
}

// Both surfaces read the same library, so neither may keep showing an entry the other removed.
function applyArrangementList(fingerprint, list) {
  computerArrangementsStore = setComputerArrangements(computerArrangementsStore, fingerprint, list);
  if (fingerprint === activeFingerprint() && isLinkActive(sharing))
    sharing = setArrangements(sharing, list);
}

// Loading from an inspector is gated and run against that computer's own list, so an enabled button
// always does something: it puts the layout into the draft the canvas is editing.
function loadComputerArrangement(fingerprint, name) {
  const store = computerArrangements(computerArrangementsStore, fingerprint);
  const entry = store.items.find((item) => item.name === name) ?? null;
  const gate = loadGate({
    isActive: fingerprint === activeFingerprint(),
    connected: isConnected(sharing),
    entry,
  });
  if (!gate.enabled || controlsBusy()) {
    sharing = { ...sharing, message: gate.reason || "The layout could not be loaded." };
    render();
    return;
  }
  if (currentSelection().kind === "settings") chosen = computerSelection(fingerprint);
  // The listed fit can be older than the displays; the draft decides, and says so when it cannot.
  sharing = loadArrangementLayout(sharing, entry.layout);
  touchSetupLink();
  render();
}

// ---------- window ----------

async function revealLogFile() {
  if (!core?.invoke) return;
  try {
    await core.invoke("reveal_logs");
  } catch (error) {
    state = { ...state, messages: [`Could not open the log file: ${nativeError(error)}`] };
    render();
  }
}

// A pinned theme lives on the root element; without one the CSS follows the system scheme.
function applyTheme(next) {
  theme = THEMES.has(next) ? next : "system";
  if (theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
}

async function loadTheme() {
  if (!core?.invoke) return;
  try {
    const view = await core.invoke("appearance_status");
    applyTheme(view?.theme);
  } catch {
    // Without a readable preference the window keeps following the system.
  }
  render();
}

async function setTheme(next) {
  if (themePending || next === theme || !THEMES.has(next)) return;
  themePending = true;
  await withThemeTransition(() => {
    applyTheme(next);
    render();
  });
  if (core?.invoke) {
    try {
      const view = await core.invoke("appearance_set_theme", { theme: next });
      applyTheme(view?.theme ?? next);
    } catch (error) {
      state = { ...state, messages: [`Could not save the theme: ${nativeError(error)}`] };
    }
  }
  themePending = false;
  render();
}

// The whole window cross-fades between themes; without view transitions it switches at once.
async function withThemeTransition(update) {
  if (reducedMotion.matches || typeof document.startViewTransition !== "function") {
    update();
    return;
  }
  document.documentElement.dataset.themeSwitch = "true";
  try {
    await document.startViewTransition(update).finished;
  } catch {
    // An interrupted transition still applied the update.
  } finally {
    delete document.documentElement.dataset.themeSwitch;
  }
}

async function hideToTray() {
  if (!core?.invoke) return;
  try {
    await core.invoke("window_hide");
  } catch (error) {
    state = { ...state, messages: [`Could not hide this window: ${nativeError(error)}`] };
    render();
  }
}

// Minimize sends MonHop to the tray; closing the window hides it there too.
async function runWindowAction(action) {
  if (action === "minimize") {
    await hideToTray();
    return;
  }
  const method = { maximize: "toggleMaximize", close: "close" }[action];
  const currentWindow = nativeWindow?.getCurrentWindow?.();
  if (!method || !currentWindow) return;
  try {
    await currentWindow[method]();
  } catch (error) {
    state = { ...state, messages: [`Window action did not finish: ${nativeError(error)}`] };
    render();
  }
}

function controlsBusy() {
  return state.busy || pairingPending || isBusyPairing(pairing) || isBusySharing(sharing);
}
