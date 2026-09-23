import {
  accessReady,
  applySnapshot,
  autoSelectInterface,
  errorMessages,
  initialState,
  nativeCheckState,
  networkReady,
  pairingOpenGate,
  selectInterface,
  selectedInterface,
  setupVerdict,
} from "./model.mjs";
import {
  applyPairingView,
  canConfirmPairing,
  canInspectPairing,
  editCandidateCode,
  initialPairingState,
  invalidatePairingCandidate,
  isBusyPairing,
  pairingFailure,
  platformLabel,
  setFingerprintCompared,
} from "./pairing-model.mjs";
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
  copyFeedbackFor,
  copyFeedbackSuccess,
  emptyCopyFeedback,
  isCopyReplyCurrent,
} from "./copy-feedback-model.mjs";
import {
  accordionManager,
  glideMove,
  glideResize,
  setPanelOpen,
  setRevealOpen,
} from "./accordion.mjs";
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
import { activeStatus, isLiveStatus } from "./computer-status.mjs";
import {
  clearForgetFor,
  keepForgetArmed,
  keepForgetListed,
  loadGate,
  pressForget,
} from "./computer-card-model.mjs";
import { forgetArrangementMotion } from "./dashboard-arrangement.mjs";
import {
  applyDimmingView,
  beginDimming,
  clampLevel,
  failDimming,
  initialDimming,
} from "./dimming-model.mjs";
import { setupSectionGates, shouldShowPairing } from "./setup-presentation.mjs";
import { setSlotPresent } from "./header-motion.mjs";
import {
  clear,
  el,
  handedOffFocus,
  icon,
  iconButton,
  nativeError,
  reducedMotion,
  setLabel,
} from "./dom.mjs";
import { canCheck, canInstall, normalizeUpdatesView } from "./updates-model.mjs";
import { normalizeAutostartView } from "./autostart-model.mjs";
import { renderReady } from "./screen-ready.mjs";
import { renderComputers } from "./screen-computers.mjs";
import { renderDisplays } from "./screen-displays.mjs";
import { renderHome } from "./screen-home.mjs";
import { renderSettings } from "./screen-settings.mjs";

const core = window.__TAURI__?.core;
const nativeWindow = window.__TAURI__?.window;
const uiCheck = window.__MONHOP_UI_CHECK__ === true;
const nativePlatform = window.__MONHOP_PLATFORM__ ?? document.documentElement.dataset.platform;
const platform = ["macos", "windows"].includes(nativePlatform) ? nativePlatform : "other";
document.documentElement.dataset.platform = platform;

const SHARING_POLL_MS = 1000;
const THEME_ORDER = ["system", "light", "dark"];
const THEME_LABELS = {
  system: "Theme follows the system",
  light: "Light theme",
  dark: "Dark theme",
};
const THEME_ICONS = { system: "monitor", light: "sun", dark: "moon" };
const PAGE_TITLES = { home: "Home", setup: "Set up", settings: "Settings" };
const PAGE_ENTER_CLEANUP_MS = 320;
const PAIRING_POLL_MS = 500;
const SECTION_PIN_MS = 2500;
const LIST_UNREADABLE = "The computer list was not recognized.";

let state = initialState(Boolean(core?.invoke));
let pairing = initialPairingState();
let pairingGeneration = 0;
let pairingPending = false;
let pairingOperation = null;
let pairingPollingAllowed = false;
let pairingPollTimer = null;
let pairingRequested = false;
let pairedKey = null;
let copyFeedback = emptyCopyFeedback();
let copyRequest = 0;
let sharing = initialSharingState();
let sharingPollTimer = null;
let sharingStatusPending = false;
let dropCopyFeedback = emptyCopyFeedback();
let dropCopyRequest = 0;
let dimming = initialDimming();
let theme = "system";
let themePending = false;
// The header icon animates once after a click, not on every poll-driven render.
let themeChanged = false;
let dimmingRequest = 0;
let dimPreview = null;
let dimmingDrag = false;
let renderAfterDrag = false;
// Harness-only: a forced starting page skips the load-driven redirect to Home below.
const forcedPage = ["home", "setup", "settings"].includes(window.MONHOP_PAGE)
  ? window.MONHOP_PAGE
  : null;
let page = forcedPage ?? "setup";
let landed = forcedPage !== null;
let settingsReturnPage = "home";
let updates = normalizeUpdatesView(null);
let updatesPending = false;
let autostart = normalizeAutostartView(null);
let autostartPending = false;
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
let pendingSection = null;
let pageTransition = null;
// Session-only choice: a completed setup returns to the last step the user opened.
let setupExpandedSection = null;
let setupExpansionChosen = false;

const nodes = {
  sections: [...document.querySelectorAll("[data-section]")],
  pageLinks: [...document.querySelectorAll("[data-page]")],
  nativeState: document.querySelector("#native-state"),
  headerConnection: document.querySelector("#header-connection"),
  headerRefresh: document.querySelector("#header-refresh"),
  headerButtons: document.querySelector("#header-buttons"),
  primaryNav: document.querySelector("#primary-nav"),
  setupVerdict: document.querySelector("#setup-verdict"),
  setupDetail: document.querySelector("#setup-detail"),
  launchAttribution: document.querySelector("#launch-attribution"),
  readyStatus: document.querySelector("#ready-status"),
  readyActions: document.querySelector("#ready-actions"),
  permissionsContent: document.querySelector("#permissions-content"),
  interfaceContent: document.querySelector("#interface-content"),
  computersStatus: document.querySelector("#computers-status"),
  computersActions: document.querySelector("#computers-actions"),
  computersContent: document.querySelector("#computers-content"),
  displaysStatus: document.querySelector("#displays-status"),
  displaysActions: document.querySelector("#displays-actions"),
  displaysContent: document.querySelector("#displays-content"),
  windowControls: document.querySelector("#window-controls"),
  homeView: document.querySelector("#home-view"),
  homeContent: document.querySelector("#home-content"),
  setupView: document.querySelector("#setup-view"),
  settingsView: document.querySelector("#settings-view"),
  settingsContent: document.querySelector("#settings-content"),
  pageTitle: document.querySelector("#page-title"),
  pageAlert: document.querySelector("#page-alert"),
  pageAlertPanel: document.querySelector("#page-alert-panel"),
};

for (const button of nodes.pageLinks)
  button.addEventListener("click", () => goToPage(button.dataset.page));
for (const button of nodes.windowControls.querySelectorAll("[data-window-action]")) {
  button.addEventListener("pointerdown", (event) => event.stopPropagation());
  button.addEventListener("dblclick", (event) => event.stopPropagation());
  button.addEventListener("click", (event) => {
    event.stopPropagation();
    runWindowAction(button.dataset.windowAction);
  });
}
nodes.windowControls.hidden = platform !== "windows";
accordionManager.mount();

for (const section of nodes.sections) {
  section.querySelector(".section-toggle").addEventListener("click", () => {
    if (section.dataset.locked === "true" || section.dataset.done !== "true") return;
    setupExpansionChosen = true;
    setupExpandedSection = section.dataset.collapsed === "true" ? section.dataset.section : null;
    render();
  });
}

for (const event of ["wheel", "touchstart", "pointerdown"])
  document.querySelector(".content-region").addEventListener(
    event,
    () => {
      pendingSection = null;
    },
    { passive: true },
  );

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
  void loadUpdatesStatus();
  // The initial page never runs through goToPage's entry hooks, so this covers a fresh
  // launch landing straight on Setup or Settings; the hooks below cover later visits.
  void loadAutostartStatus();
  listenUpdates();
}

function onReturn() {
  if (!refreshAfterReturn) return;
  refreshAfterReturn = false;
  if (page === "setup") void requestAutomaticSnapshot("focus");
  void refreshSharingStatus();
  void loadComputers();
}

// ---------- navigation ----------

function goToPage(nextPage, after) {
  if (!["home", "setup", "settings"].includes(nextPage)) return;
  const entered = page !== nextPage;
  landed = true;
  if (!entered) {
    after?.();
    return;
  }
  pendingSection = null;
  const commit = () => {
    page = nextPage;
    renderPageVisibility();
    document.querySelector(".content-region").scrollTop = 0;
  };
  const finish = () => {
    after?.();
    onPageEntered(nextPage);
    render();
  };
  const animate =
    !uiCheck &&
    !reducedMotion.matches &&
    document.visibilityState === "visible" &&
    typeof document.startViewTransition === "function";
  if (!animate) {
    document.documentElement.dataset.pageEnter = "true";
    commit();
    window.requestAnimationFrame(() => {
      finish();
      window.setTimeout(
        () => delete document.documentElement.dataset.pageEnter,
        PAGE_ENTER_CLEANUP_MS,
      );
    });
    return;
  }
  pageTransition?.skipTransition();
  const transition = document.startViewTransition(commit);
  pageTransition = transition;
  void transition.finished
    .catch(() => {})
    .finally(() => {
      if (pageTransition !== transition) return;
      pageTransition = null;
      finish();
    });
}

function onPageEntered(nextPage) {
  if (nextPage === "setup") onEnterSetup();
  else if (nextPage === "settings") {
    void loadUpdatesStatus();
    void loadAutostartStatus();
  } else {
    leavePairing();
    if (isEditingLayout(sharing)) void endLayoutEdit();
    void loadComputers();
  }
}

// The gear is a toggle: pressing it again from Settings returns to wherever it was opened from.
function toggleSettings() {
  if (page === "settings") goToPage(settingsReturnPage);
  else {
    settingsReturnPage = page;
    goToPage("settings");
  }
}

// A code under review or a running check holds the sharing port; leaving the exchange abandons it
// so the supervisor can reconnect. Retrying means checking the other computer's code again.
function leavePairing() {
  pairingRequested = false;
  if (pairing.view?.candidateId != null) void cancelPairing();
}

function onEnterSetup() {
  void requestAutomaticSnapshot("entry");
  openPairingOnExplicitEntry();
  void loadAutostartStatus();
}

function scrollToSection(key) {
  pendingSection = { key, until: Date.now() + SECTION_PIN_MS };
  pinPendingSection();
}

// The section's content arrives over the next few status polls and changes height under the user, so
// the request is re-applied after every render until it settles or the user scrolls somewhere else.
function pinPendingSection() {
  if (!pendingSection) return;
  const section = nodes.sections.find((node) => node.dataset.section === pendingSection.key);
  const content = document.querySelector(".content-region");
  if (!section || !content || Date.now() > pendingSection.until) {
    pendingSection = null;
    return;
  }
  content.scrollTop += section.getBoundingClientRect().top - content.getBoundingClientRect().top;
}

// ---------- render ----------

function captureInteraction() {
  const content = document.querySelector(".content-region");
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
  return { scrollTop: content?.scrollTop ?? 0, focused };
}

function restoreInteraction(interaction) {
  const content = document.querySelector(".content-region");
  if (content) content.scrollTop = interaction.scrollTop;
  if (!interaction.focused) return;
  const focused = interaction.focused.id
    ? document.getElementById(interaction.focused.id)
    : [...document.querySelectorAll("[data-focus-key]")].find(
        (node) => node.dataset.focusKey === interaction.focused.focusKey,
      );
  // A control inside a section that locked itself cannot take focus back; one inside a block that
  // closed goes where that block handed its focus.
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

function readyDone() {
  return Boolean(state.nativeAvailable && accessReady(state.snapshot) && networkReady(state));
}

function context() {
  const active = activeFingerprint();
  const activeComputer = findComputer(computers, active);
  const status = activeStatus({
    computers,
    sharingView: sharing.view,
    active,
    nativeAvailable: state.nativeAvailable,
    localName: platformLabel(platform, true),
  });
  return {
    core: Boolean(core?.invoke),
    platform,
    uiCheck,
    state,
    pairing,
    sharing,
    computers,
    computerArrangements: computerArrangementsStore,
    layoutForget,
    active,
    activeComputer,
    status,
    gates: setupSectionGates({
      readyDone: readyDone(),
      pairedCount: computers.items.length,
      activeName: activeComputer ? displayName(activeComputer) : "",
      displaysReady: isLiveStatus(status),
      layoutSaved: activeComputer?.setup?.saved === true,
    }),
    snapshotCheck,
    pairingPending,
    pairingOperation,
    computersLoadPending: computersLoad.running,
    computersLoadFailure,
    renaming,
    renamePending,
    renameDraft,
    forgetConfirmed,
    copyFeedback: copyFeedbackFor(copyFeedback, pairingCodeSubject()),
    dropCopyFeedback,
    dimming,
    updates: { view: updates, pending: updatesPending },
    autostart: { view: autostart, pending: autostartPending },
    busy: controlsBusy(),
    peerName: peerName(activeComputer),
    gate: pairingOpenGate(state),
    showPairing: shouldShowPairing({
      requested: pairingRequested,
      pairedCount: computers.items.length,
      phase: pairing.view?.phase,
    }),
    actions: {
      goToPage,
      refreshSnapshot,
      requestPermissions,
      requestWifiPermission,
      openSettings,
      chooseInterface,
      openPairing,
      beginPairing,
      dismissPairing,
      createPairingIdentity,
      inspectPairingCode,
      confirmPairing,
      requestNetworkAccess,
      cancelPairing,
      copyPairingCode,
      copyLastDrop,
      setDimmingEnabled,
      previewDimLevel,
      setDimLevel,
      toggleDimming,
      beginDimDrag,
      endDimDrag,
      editCandidate,
      toggleCompared,
      useComputer,
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
  // The Displays section hands back the same editor across polls; only a view it dropped is torn down.
  const previousArrangementView = arrangementView;
  arrangementView = null;
  const ctx = context();
  nodes.nativeState.dataset.checkState = nativeCheckState(state);
  setLabel(nodes.nativeState, ctx.status.label);
  nodes.headerConnection.dataset.tone = ctx.status.tone;
  renderPageChrome(ctx);
  renderPageAlert(ctx);
  renderSections(ctx);
  renderReady(nodes, ctx);
  renderComputers(nodes, ctx);
  renderDisplays(nodes, ctx);
  renderHome(nodes, ctx);
  renderSettings(nodes, ctx);
  applySharedTransitionNames();
  restoreInteraction(interaction);
  pinPendingSection();
  if (previousArrangementView && previousArrangementView !== arrangementView)
    previousArrangementView.destroy();
  syncArrangements();
}

function renderPageChrome(ctx) {
  renderPageVisibility();
  clear(nodes.headerButtons);
  nodes.headerButtons.append(
    iconButton({
      id: "settings-gear",
      label: "Settings",
      iconName: "settings",
      variant: "ghost",
      size: "sm",
      onClick: toggleSettings,
    }),
  );
  const themeToggle = iconButton({
    id: "theme-toggle",
    label: THEME_LABELS[theme],
    iconName: THEME_ICONS[theme],
    variant: "ghost",
    size: "sm",
    onClick: cycleTheme,
  });
  themeToggle.classList.add("theme-toggle");
  if (themeChanged) {
    themeToggle.dataset.changed = "true";
    themeChanged = false;
  }
  nodes.headerButtons.append(themeToggle);
  if (ctx.core)
    nodes.headerButtons.append(
      iconButton({
        id: "hide-to-tray",
        label: "Hide to the menu bar",
        iconName: "x",
        variant: "ghost",
        size: "sm",
        onClick: hideToTray,
      }),
    );
}

// The transition callback only flips page visibility, the named tab marker and the header's page
// action, which moves with the tab. Full page work resumes after the compositor has captured both pages.
function renderPageVisibility() {
  nodes.pageTitle.textContent = `MonHop · ${PAGE_TITLES[page]}`;
  nodes.homeView.hidden = page !== "home";
  nodes.setupView.hidden = page !== "setup";
  nodes.settingsView.hidden = page !== "settings";
  for (const button of nodes.pageLinks) {
    button.setAttribute("aria-current", button.dataset.page === page ? "page" : "false");
    const indicator = button.querySelector(".nav-active-indicator");
    if (indicator)
      indicator.style.viewTransitionName = button.dataset.page === page ? "nav-active-tab" : "none";
  }
  renderHeaderRefresh();
  applySharedTransitionNames();
}

// Home's only page action sits in the header beside the connection status. Its slot outlives
// renders, so it pops in and out while the status pill glides aside.
function renderHeaderRefresh() {
  const home = page === "home";
  if (home)
    nodes.headerRefresh.replaceChildren(
      iconButton({
        id: "computers-refresh",
        label: "Check the paired computers again",
        variant: "ghost",
        size: "sm",
        disabled: computersLoad.running,
        busy: computersLoad.running,
        onClick: loadComputers,
      }),
    );
  setSlotPresent(nodes.headerRefresh, home, [nodes.headerConnection], {
    focusTarget: home ? null : pageFocusAnchor(),
  });
}

function applySharedTransitionNames() {
  for (const node of document.querySelectorAll("[data-shared-transition]")) {
    const pageNode = node.closest(".app-page");
    node.style.viewTransitionName =
      pageNode && !pageNode.hidden ? node.dataset.sharedTransition : "none";
  }
}

// A locked section is washed out, inert, and says in its own header what is still missing.
function renderSections(ctx) {
  const lines = {
    ready: readyLine(ctx.gates.ready.done),
    computers: ctx.gates.computers.locked ? ctx.gates.computers.reason : computersLine(ctx),
    displays: displaysLine(ctx),
  };
  const expanded = setupExpansionChosen ? setupExpandedSection : defaultSetupSection(ctx.gates);
  for (const section of nodes.sections) {
    const key = section.dataset.section;
    const gate = ctx.gates[key];
    const collapsed = !gate.locked && gate.done && key !== expanded;
    const still = section.dataset.presented !== "true" || nodes.setupView.hidden;
    const header = section.querySelector(".section-header");
    const status = section.querySelector(".section-status");
    // Folding moves the status line beside the title; header and line glide from where they were drawn.
    const drawn =
      !still && section.dataset.collapsed !== String(collapsed)
        ? { header: header.getBoundingClientRect().height, status: status.getBoundingClientRect() }
        : null;
    section.dataset.locked = String(gate.locked);
    section.dataset.done = String(gate.done);
    section.dataset.collapsed = String(collapsed);
    section.dataset.presented = "true";
    status.textContent = lines[key];
    if (drawn) {
      glideResize(header, drawn.header);
      glideMove(status, drawn.status);
    }
    const body = section.querySelector(".section-body");
    body.inert = collapsed || gate.locked;
    setPanelOpen(body, !collapsed, { instant: still });
    const toggle = section.querySelector(".section-toggle");
    const chevron = section.querySelector(".section-chevron");
    toggle.hidden = gate.locked || !gate.done;
    chevron.hidden = toggle.hidden;
    toggle.disabled = gate.locked || !gate.done;
    toggle.setAttribute("aria-expanded", String(!collapsed));
    toggle.setAttribute("aria-label", `${collapsed ? "Expand" : "Collapse"} ${gate.title}`);
    const marker = section.querySelector(".section-index");
    const mark = gate.done ? "check" : String(gate.index + 1);
    if (marker.dataset.mark !== mark) {
      marker.dataset.mark = mark;
      clear(marker);
      if (mark === "check") marker.append(icon("check", 11));
      else marker.textContent = mark;
    }
  }
}

// The header line is the verdict, plus the network it settled on once everything passed.
function readyLine(done) {
  const verdict = setupVerdict(state, { checking: snapshotCheck.pending });
  if (!done) return verdict.label;
  const selected = selectedInterface(state);
  return selected ? `Ready · ${selected.networkName || selected.name}` : verdict.label;
}

function computersLine({ computers: list, activeComputer }) {
  if (list.items.length === 0) return "None paired yet";
  const count = `${list.items.length} paired`;
  return activeComputer
    ? `${count} · ${displayName(activeComputer)} in use`
    : `${count} · none in use`;
}

function displaysLine({ gates, activeComputer }) {
  if (gates.displays.locked) return gates.displays.reason;
  return activeComputer?.setup?.saved === true
    ? "Layout saved on both computers"
    : "Not arranged yet";
}

function defaultSetupSection(gates) {
  for (const key of ["ready", "computers", "displays"])
    if (!gates[key].locked && !gates[key].done) return key;
  return "displays";
}

// Where focus goes when the control holding it leaves the page: the header control of the page
// on screen, already rebuilt for this render.
function pageFocusAnchor() {
  const link = nodes.pageLinks.find((button) => button.dataset.page === page);
  return link ?? document.getElementById("settings-gear");
}

function renderPageAlert(ctx) {
  const stateMessages = errorMessages(state);
  const messages = [...stateMessages];
  const pairingOnScreen = page === "setup" && ctx.showPairing;
  const displaysOnScreen = page === "setup" && isConnected(sharing);
  if (pairing.view?.phase === "error" && pairing.view.message && !pairingOnScreen)
    messages.push(pairing.view.message);
  if (pairing.message && !pairingOnScreen) messages.push(pairing.message);
  if (sharing.message && !displaysOnScreen) messages.push(sharing.message);
  if (computersLoadFailure) messages.push(computersLoadFailure);
  const unique = [...new Set(messages.filter(Boolean))];
  // A leaving alert keeps its last words and tone while it glides shut.
  if (!unique.length) {
    setRevealOpen(nodes.pageAlertPanel, false, { focusTarget: pageFocusAnchor() });
    return;
  }
  nodes.pageAlert.dataset.tone =
    stateMessages.length ||
    computersLoadFailure ||
    pairing.message ||
    pairing.view?.phase === "error" ||
    sharing.view?.phase === "error"
      ? "failure"
      : "notice";
  clear(nodes.pageAlert);
  nodes.pageAlert.append(el("span", { className: "page-alert-copy", text: unique.join(" ") }));
  if (computersLoadFailure)
    nodes.pageAlert.append(
      iconButton({
        id: "computers-retry",
        label: "Read the paired computers again",
        disabled: computersLoad.running,
        busy: computersLoad.running,
        size: "sm",
        onClick: loadComputers,
      }),
    );
  if (stateMessages.length || sharing.message)
    nodes.pageAlert.append(
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
  setRevealOpen(nodes.pageAlertPanel, true);
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
    if (page === "setup") openPairingOnExplicitEntry();
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
  const selectedContext = selectedNetworkContextKey(selectedInterface(state));
  state = autoSelectInterface(applySnapshot(nextState, snapshot), computers.interfaceId);
  if (selectedNetworkContextKey(selectedInterface(state)) !== selectedContext)
    pairing = invalidatePairingCandidate(pairing);
  snapshotCheck = { pending: false, ...snapshotCheckResult(state.snapshot, freshness) };
}

function chooseInterface(id) {
  if (controlsBusy() || isSessionActive(sharing)) return;
  const previous = state.selectedInterfaceId;
  state = selectInterface(state, id);
  if (state.selectedInterfaceId !== previous) pairing = invalidatePairingCandidate(pairing);
  render();
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
  const selected = selectedInterface(state);
  if (!selected) return "";
  return JSON.stringify([
    selected.id,
    selected.address,
    selected.prefixLength,
    selected.networkName,
    selected.attachmentKnown,
    selected.up,
    selected.physical,
    state.snapshot?.platform,
  ]);
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
  await invokePairing("pairing_open", { interfaceId: state.selectedInterfaceId });
}

// The list stays the focus once a computer is paired; the code exchange opens on request.
function beginPairing() {
  pairingRequested = true;
  if (!pairing.view || ["closed", "paired"].includes(pairing.view.phase)) void openPairing();
  else render();
}

function dismissPairing() {
  leavePairing();
  render();
}

async function createPairingIdentity() {
  if (!pairingOpenGate(state).allowed) return;
  await invokePairing("pairing_create_identity", { interfaceId: state.selectedInterfaceId });
}

function editCandidate(value) {
  pairing = editCandidateCode(pairing, value);
  render();
}

function toggleCompared() {
  pairing = setFingerprintCompared(pairing, !pairing.compared);
  render();
}

async function inspectPairingCode() {
  if (!canInspectPairing(pairing) || controlsBusy()) return;
  await invokePairing("pairing_inspect", { code: pairing.candidateCode });
}

async function confirmPairing() {
  if (!canConfirmPairing(pairing) || controlsBusy()) return;
  pairingPollingAllowed = true;
  await invokePairing("pairing_confirm", { candidateId: pairing.view.candidateId });
}

async function requestNetworkAccess() {
  if (state.snapshot?.platform !== "macos" || !canConfirmPairing(pairing) || controlsBusy()) return;
  pairingPollingAllowed = true;
  await invokePairing("pairing_request_network_access", { candidateId: pairing.view.candidateId });
}

async function cancelPairing() {
  await invokePairing("pairing_cancel");
}

// The copy subject bundles the code with the pairing generation, so any other pairing operation
// starting (even one that leaves the code text unchanged) drops this feedback as stale too.
function pairingCodeSubject() {
  return { localCode: pairing.view?.localCode, pairingGeneration };
}

async function copyPairingCode() {
  const localCode = pairing.view?.localCode;
  const subject = pairingCodeSubject();
  if (!localCode || copyFeedbackFor(copyFeedback, subject).state === "pending") return;
  const request = ++copyRequest;
  const isCurrent = () => isCopyReplyCurrent(request, copyRequest, subject, pairingCodeSubject());
  if (!core?.invoke) {
    copyFeedback = copyFeedbackFailure(subject, request, "Open MonHop to copy this code.");
    render();
    return;
  }
  copyFeedback = beginCopyFeedback(subject, request);
  render();
  try {
    await core.invoke("pairing_copy_code");
    if (!isCurrent()) return;
    copyFeedback = copyFeedbackSuccess(subject, request, "Copied. Paste it on the other computer.");
    window.setTimeout(() => {
      if (isCurrent()) {
        copyFeedback = emptyCopyFeedback();
        render();
      }
    }, 1500);
  } catch (error) {
    if (!isCurrent()) return;
    copyFeedback = copyFeedbackFailure(subject, request, `Could not copy: ${nativeError(error)}`);
  }
  render();
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
    if (["pairing_inspect", "pairing_confirm"].includes(command))
      pairing = invalidatePairingCandidate(pairing);
    pairing = pairingFailure(pairing, nativeError(error));
    pairingPollingAllowed = false;
  } finally {
    if (generation === pairingGeneration) {
      pairingPending = false;
      pairingOperation = null;
      render();
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

function applyPairingResult(result) {
  const current = pairing;
  const next = applyPairingView(current, result);
  if (current.view?.localCode !== next.view?.localCode) {
    copyRequest += 1;
    copyFeedback = emptyCopyFeedback();
  }
  pairing = next;
  // One exchange ends once: the new computer joins the list and the supervisor connects to it.
  // Leaving `paired` re-arms it, so pairing the same computer again is still a new exchange.
  const key = pairing.view?.phase === "paired" ? pairing.view.peerFingerprint : null;
  if (!key) {
    pairedKey = null;
    return;
  }
  if (key === pairedKey) return;
  pairedKey = key;
  pairingRequested = false;
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
  // The first reply decides where a launch lands: Setup with nothing paired, Home otherwise.
  if (landed) return;
  landed = true;
  if (next.items.length > 0) page = "home";
}

function startRename(fingerprint, scope) {
  renaming = fingerprint;
  renameDraft = findComputer(computers, fingerprint)?.name ?? "";
  render();
  const field = document.getElementById(`${scope}-rename-field-${fingerprint}`);
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

async function beginLayoutEdit(fingerprint = activeFingerprint()) {
  const interfaceId = state.selectedInterfaceId;
  if (!fingerprint || !canEditLayout(sharing, interfaceId)) return;
  await runSharing("edit", () => core.invoke("sharing_edit_begin", { interfaceId, fingerprint }));
}

async function endLayoutEdit() {
  if (!isEditingLayout(sharing)) return;
  await runSharing("edit-end", () => core.invoke("sharing_edit_end"));
}

function changeLayout() {
  const fingerprint = activeFingerprint();
  goToPage("setup", () => scrollToSection("displays"));
  void beginLayoutEdit(fingerprint);
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
  if (sharingPollTimer !== null || !core?.invoke || !shouldPollSharing(sharing, computers.active))
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

// Loading from a card is gated and run against that card's own list, so an enabled button always
// does something: it opens the Displays editor and puts the layout into the draft there.
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
  goToPage("setup", () => scrollToSection("displays"));
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
  theme = THEME_ORDER.includes(next) ? next : "system";
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

async function cycleTheme() {
  if (themePending) return;
  themePending = true;
  themeChanged = true;
  const next = THEME_ORDER[(THEME_ORDER.indexOf(theme) + 1) % THEME_ORDER.length];
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

async function runWindowAction(action) {
  const method = { minimize: "minimize", maximize: "toggleMaximize", close: "close" }[action];
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
