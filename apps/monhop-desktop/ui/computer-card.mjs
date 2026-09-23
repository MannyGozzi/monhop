import { ACCORDION_TOGGLE, createAccordion } from "./accordion.mjs";
import { computerStatus } from "./computer-status.mjs";
import { computerArrangements, displayName } from "./computers-model.mjs";
import { platformLabel } from "./pairing-model.mjs";
import { displayNoticeCopy, isConnected, noticePresentation } from "./sharing-model.mjs";
import { createDashboardArrangement } from "./dashboard-arrangement.mjs";
import {
  controlSwitchRows,
  displaysFreshness,
  layoutChips,
  layoutRows,
} from "./computer-card-model.mjs";
import {
  button,
  card,
  el,
  icon,
  iconButton,
  motionEase,
  motionEnabled,
  motionMs,
  motionToken,
  note,
  platformGlyph,
  presence,
  reducedMotion,
  row,
  rows,
  settle,
  sinceChanged,
  statusChip,
  swap,
  switchRow,
} from "./dom.mjs";
import { orbitShown, pillChange, pillLook, tweenTiming } from "./sharing-pill-model.mjs";

// The longest of the play/stop animations. A card rebuilt inside this window starts its motion
// where the last one left off, so a status poll mid-morph does not replay it from the top.
const USE_MOTION_MS = 700;
// A Layouts list rebuilt within this window is still the same entrance, stagger included.
const LIST_ENTER_MS = 600;

// One computer rendering keeps Home and Set up in sync while each page owns its own actions.
export function computerCard(
  ctx,
  computer,
  { scope, extras = [], details = [], viewport = true } = {},
) {
  const { busy, renaming, renamePending, active } = ctx;
  const fingerprint = computer.fingerprint;
  const name = displayName(computer);
  const local = localName(ctx);
  const status = computerStatus(computer, ctx.sharing.view, active, local);
  const inUse = fingerprint === active;
  const pending = renamePending === fingerprint;
  const editing = renaming === fingerprint || pending;
  const key = `${scope}-${fingerprint}`;

  const children = [
    el("div", {
      className: "computer-card",
      children: [
        el("div", {
          className: "computer-identity",
          children: [
            el("span", {
              className: "computer-icon-wrap",
              children: [platformGlyph(computer.platform)],
            }),
            el("div", {
              className: "computer-copy",
              children: [
                swap(
                  `${key}-name`,
                  editing
                    ? nameField(ctx, computer, scope, name, pending)
                    : nameButton(ctx, computer, scope, name),
                  editing ? "edit" : "view",
                ),
                el("span", {
                  className: "computer-meta",
                  text: editing
                    ? pending
                      ? "Saving…"
                      : "Enter to save · Esc to cancel"
                    : [platformLabel(computer.platform), computer.address]
                        .filter(Boolean)
                        .join(" · "),
                }),
              ],
            }),
          ],
        }),
        el("div", {
          className: "computer-actions",
          children: [
            scope === "home"
              ? sharingPill(ctx, fingerprint, name, inUse)
              : useToggle(ctx, fingerprint, scope, inUse),
          ],
        }),
      ],
    }),
    presence(
      `${key}-detail`,
      scope === "home" && inUse && status.key === "sharing" ? note(status.detail) : null,
    ),
    presence(
      `${key}-control`,
      scope === "home" && inUse ? controlSwitches(ctx, computer, name, local) : null,
    ),
    presence(`${key}-notice`, noticeLine(ctx, computer, inUse)),
    presence(`${key}-viewport`, viewport ? cardArrangement(ctx, computer, key) : null),
    presence(
      `${key}-extras`,
      extras.some(Boolean)
        ? el("div", { className: "card-extras", children: extras.filter(Boolean) })
        : null,
    ),
    layoutsDisclosure(ctx, computer, key),
    createAccordion(
      `${scope}-computer-${fingerprint}`,
      "computer-details",
      "Details",
      ...details,
      ...forgetControls(ctx, computer, name),
    ),
  ];
  const node = card({ children: children.filter(Boolean), tone: status.tone });
  // The live edge fades in only when the computer goes live, not on every re-render while it is.
  const live = status.tone === "active";
  const liveElapsed = sinceChanged(`${key}-live`, live);
  if (live && liveElapsed < USE_MOTION_MS) {
    node.dataset.liveEnter = "true";
    node.style.setProperty("--live-delay", `${-Math.round(liveElapsed)}ms`);
  }
  const identity = node.querySelector(".computer-identity");
  if (inUse && ["home", "setup"].includes(scope) && identity)
    identity.dataset.sharedTransition = "active-computer-identity";
  if (scope === "home" && inUse) node.classList.add("home-hero");
  if (busy) node.dataset.busy = "true";
  return node;
}

// Each computer's Link capsule outlives renders: Home is rebuilt whenever a status poll changes it,
// and a kept node keeps its tweens, orbit, hover and focus through each rebuild.
const pills = new Map();
const PILL_LABELS = { start: "Start sharing", sharing: "Sharing", pause: "Pause" };
const GLYPH = ["opacity", "transform"];
let pillsQueued = false;
reducedMotion.addEventListener("change", () => {
  for (const pill of pills.values()) {
    stillPill(pill);
    pill.shown = null;
  }
  queuePills();
});

function sharingPill(ctx, fingerprint, name, inUse) {
  const pill = pills.get(fingerprint) ?? buildPill(fingerprint);
  pills.set(fingerprint, pill);
  const control = pill.button;
  control.setAttribute("aria-label", `${inUse ? "Pause sharing" : "Start sharing"} with ${name}`);
  control.setAttribute("aria-pressed", String(inUse));
  if (ctx.busy) control.setAttribute("aria-busy", "true");
  else control.removeAttribute("aria-busy");
  control.disabled = ctx.busy;
  pill.press = () => ctx.actions.useComputer(inUse ? null : fingerprint);
  pill.want = { ...pill.want, inUse, busy: ctx.busy };
  queuePills();
  return pill.wrap;
}

function pillLayer(className, children) {
  return el("span", { className, children });
}

function buildPill(fingerprint) {
  const [orbitSlow, orbitFast, pulse] = [
    pillLayer("sharing-pill-orbit slow"),
    pillLayer("sharing-pill-orbit fast"),
    pillLayer("sharing-pill-pulse"),
  ];
  const [halo, tint, edge, ring, sweep] = [
    pillLayer("sharing-pill-halo"),
    pillLayer("sharing-pill-tint"),
    pillLayer("sharing-pill-edge"),
    pillLayer("sharing-pill-ring", [orbitSlow, orbitFast]),
    pillLayer("sharing-pill-sweep"),
  ];
  const glyphs = [
    ["node", pillLayer("sharing-pill-node local")],
    ["node", pillLayer("sharing-pill-node peer")],
    ["dot", pillLayer("sharing-pill-dot", [pulse])],
    ["bar", pillLayer("sharing-pill-bar first")],
    ["bar", pillLayer("sharing-pill-bar second")],
  ];
  const labels = Object.entries(PILL_LABELS).map(([kind, text]) =>
    el("span", { className: kind, text }),
  );
  const mark = el("span", {
    className: "sharing-pill-mark",
    attrs: { "aria-hidden": "true" },
    children: glyphs.map(([, node]) => node),
  });
  const control = el("button", {
    className: "sharing-pill",
    attrs: { type: "button" },
    dataset: { focusKey: `sharing-pill-${fingerprint}` },
    children: [
      pillLayer("sharing-pill-glass"),
      tint,
      edge,
      sweep,
      ring,
      mark,
      el("span", {
        className: "sharing-pill-label",
        attrs: { "aria-hidden": "true" },
        children: labels,
      }),
    ],
  });
  const wrap = el("span", { className: "sharing-pill-wrap", children: [halo, control] });
  const parts = [
    [control, "capsule", ["width"]],
    [halo, "halo"],
    [tint, "tint"],
    [edge, "edge"],
    [ring, "ring"],
    [orbitSlow, "orbit"],
    [orbitFast, "orbit"],
    [mark, "mark"],
    ...glyphs.map(([part, node]) => [node, part, GLYPH]),
    ...labels.map((node) => [node, "label", GLYPH]),
  ].map(([node, part, properties = ["opacity"]]) => ({ node, part, properties }));
  const pill = {
    key: fingerprint,
    wrap,
    button: control,
    ring,
    sweep,
    orbitSlow,
    orbitFast,
    pulse,
    parts,
    want: { inUse: false, busy: false, hover: false, focus: false, rested: false },
    shown: null,
    tweens: [],
    loops: [],
    sweepTween: null,
    pressTween: null,
    pressed: false,
    press: null,
  };
  const engage = (change) => {
    pill.want = { ...pill.want, ...change };
    queuePills();
  };
  // A render re-inserts the kept node, which re-fires enter and focus but never a leave, so only a
  // real leave or a move of focus elsewhere ends the rest that follows a press.
  wrap.addEventListener("pointerenter", () => engage({ hover: true }));
  wrap.addEventListener("pointerleave", () => engage({ hover: false, rested: false }));
  control.addEventListener("focus", () => engage({ focus: control.matches(":focus-visible") }));
  control.addEventListener("blur", (event) =>
    engage(event.relatedTarget ? { focus: false, rested: false } : { focus: false }),
  );
  control.addEventListener("click", () => {
    engage({ rested: true });
    pill.press?.();
  });
  control.addEventListener("pointerdown", (event) => {
    if (event.button === 0) pressPill(pill, true);
  });
  for (const type of ["pointerup", "pointercancel", "pointerleave"])
    control.addEventListener(type, () => pressPill(pill, false));
  return pill;
}

// One pass after the render (or event) that asked for it, once the capsules are back in the page:
// the look they show is read there before the new one is applied, so every change tweens from it.
function queuePills() {
  if (pillsQueued) return;
  pillsQueued = true;
  queueMicrotask(() => {
    pillsQueued = false;
    for (const pill of pills.values()) syncPill(pill);
  });
}

function syncPill(pill) {
  if (!pill.wrap.isConnected) {
    stillPill(pill);
    pills.delete(pill.key);
    return;
  }
  const next = pillLook(pill.want);
  const change = pillChange(pill.shown, next);
  if (!change) return;
  const motion = change.animate && motionEnabled() && pill.wrap.getClientRects().length > 0;
  const from = motion ? readParts(pill) : null;
  for (const tween of pill.tweens) tween.cancel();
  pill.tweens = [];
  pill.wrap.dataset.state = next.state;
  pill.wrap.dataset.busy = String(next.busy);
  pill.wrap.dataset.pause = String(next.pause);
  pill.wrap.dataset.lean = String(next.lean);
  pill.shown = next;
  if (motion) pill.tweens = tweenParts(pill, from, readParts(pill));
  if (motion && change.sweep) {
    pill.sweepTween?.cancel();
    pill.sweepTween = pill.sweep.animate(
      { transform: ["translateX(-100%)", "translateX(100%)"] },
      { duration: motionMs("--motion-sweep"), easing: motionEase("--ease-in-out") },
    );
  }
  runOrbit(pill, !reducedMotion.matches && orbitShown(next));
}

function readParts(pill) {
  return pill.parts.map(({ node, properties }) => {
    const style = getComputedStyle(node);
    return Object.fromEntries(properties.map((property) => [property, style[property]]));
  });
}

// Each value that differs glides from what was on screen, so a change mid-tween retargets.
function tweenParts(pill, from, to) {
  const tweens = [];
  for (const [index, { node, part, properties }] of pill.parts.entries()) {
    const rising = Number(to[index].opacity) > Number(from[index].opacity);
    for (const property of properties) {
      if (from[index][property] === to[index][property]) continue;
      const timing = tweenTiming({ part, property, rising });
      const tween = node.animate(
        { [property]: [from[index][property], to[index][property]] },
        {
          duration: motionMs(timing.duration),
          easing: motionEase(timing.easing),
          delay: timing.delay ? motionMs(timing.delay) : 0,
          fill: "both",
        },
      );
      void settle(tween);
      tweens.push(tween);
    }
  }
  return tweens;
}

// The orbit, the busy arc and the dot's breath loop on the document clock, so no render restarts
// them. Leaving, they keep turning until the ring has faded.
function runOrbit(pill, on) {
  if (on) {
    if (!pill.loops.length)
      pill.loops = [
        loop(pill.orbitSlow, ORBIT_TURN, "--loop-orbit", "linear"),
        loop(pill.orbitFast, ORBIT_TURN, "--loop-orbit-busy", "linear"),
        loop(pill.pulse, livePulse(), "--loop-live", motionEase("--ease-out")),
      ];
    return;
  }
  const fade = pill.tweens.find((tween) => tween.effect.target === pill.ring);
  if (!fade) {
    stopLoops(pill);
    return;
  }
  void settle(fade, () => {
    if (!orbitShown(pill.shown)) stopLoops(pill);
  });
}

const ORBIT_TURN = { transform: ["rotate(0turn)", "rotate(1turn)"] };

function livePulse() {
  const scale = motionToken("--scale-live-ring");
  return {
    opacity: [Number(motionToken("--opacity-live-ring")), 0],
    transform: ["none", `scale(${scale})`],
  };
}

function loop(node, keyframes, duration, easing) {
  const animation = node.animate(keyframes, {
    duration: motionMs(duration),
    easing,
    iterations: Infinity,
  });
  animation.startTime = 0;
  return animation;
}

function stopLoops(pill) {
  for (const animation of pill.loops) animation.cancel();
  pill.loops = [];
}

function stillPill(pill) {
  for (const animation of [...pill.tweens, pill.sweepTween, pill.pressTween]) animation?.cancel();
  pill.tweens = [];
  stopLoops(pill);
}

// Press is quick and release springs back, both on the kept node so a render mid-press keeps them.
function pressPill(pill, down) {
  if (down === pill.pressed || (down && pill.button.disabled)) return;
  pill.pressed = down;
  const from = getComputedStyle(pill.button).transform;
  pill.pressTween?.cancel();
  pill.pressTween = pill.button.animate(
    { transform: [from, down ? `scale(${motionToken("--scale-press")})` : "none"] },
    {
      duration: motionEnabled() ? motionMs(down ? "--motion-instant" : "--motion-spring") : 0,
      easing: motionEase(down ? "--ease-out" : "--ease-spring"),
      fill: "forwards",
    },
  );
  if (!down) void settle(pill.pressTween);
}

// One button for both states, so pressing it keeps the focus and the glyph morphs in place
// instead of one control being swapped for another.
function useToggle(ctx, fingerprint, scope, inUse) {
  const { actions, busy } = ctx;
  const state = inUse ? "sharing" : "idle";
  const node = iconButton({
    id: `${scope}-use-${fingerprint}`,
    label: inUse ? "Pause sharing with this computer" : "Use this computer",
    art: useGlyphs(inUse),
    size: "sm",
    disabled: busy,
    onClick: () => actions.useComputer(inUse ? null : fingerprint),
  });
  node.classList.add("use-toggle");
  node.dataset.state = state;
  const elapsed = sinceChanged(`${scope}-use-state-${fingerprint}`, state);
  if (elapsed < USE_MOTION_MS) {
    node.dataset.enter = "true";
    node.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
  }
  return node;
}

// Both glyphs are always drawn and the current one is marked, so CSS alone cross-fades and turns
// one into the other; the fill and the ring are the button's surface under them.
function useGlyphs(inUse) {
  return el("span", {
    className: "use-glyphs",
    attrs: { "aria-hidden": "true" },
    children: [
      el("span", { className: "use-fill" }),
      el("span", { className: "use-ring" }),
      useGlyph("play", !inUse),
      useGlyph("square", inUse),
    ],
  });
}

function useGlyph(name, current) {
  return el("span", {
    className: "use-glyph",
    dataset: { current: String(current) },
    children: [icon(name, 14)],
  });
}

// The name reads as text and edits in place: a pencil appears on hover and focus, and the
// hint line below says how to finish.
function nameButton(ctx, computer, scope, name) {
  const node = el("button", {
    className: "computer-name",
    attrs: { type: "button", title: "Click to rename", "aria-label": `Rename ${name}` },
    children: [el("span", { text: name }), icon("pencil", 12)],
  });
  node.dataset.focusKey = `${scope}-name-${computer.fingerprint}`;
  node.addEventListener("click", () => ctx.actions.startRename(computer.fingerprint, scope));
  return node;
}

function nameField(ctx, computer, scope, name, pending) {
  const { actions } = ctx;
  const field = el("input", {
    className: "computer-name-input",
    attrs: {
      id: `${scope}-rename-field-${computer.fingerprint}`,
      maxlength: 48,
      placeholder: name,
      "aria-label": `Name for ${name}`,
      autocomplete: "off",
      spellcheck: "false",
    },
  });
  field.value = ctx.renameDraft ?? computer.name;
  field.addEventListener("input", () => actions.draftRename(field.value));
  field.disabled = pending;
  field.dataset.focusKey = `${scope}-rename-field-${computer.fingerprint}`;
  // Enter and Escape settle the edit themselves; the blur they cause must not settle it again.
  let settled = pending;
  const commit = () => {
    if (settled) return;
    settled = true;
    const next = field.value.trim();
    if (next && next !== computer.name) actions.renameComputer(computer.fingerprint, next);
    else actions.cancelRename();
  };
  field.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      commit();
    } else if (event.key === "Escape") {
      event.preventDefault();
      settled = true;
      actions.cancelRename();
    }
  });
  field.addEventListener("blur", commit);
  return field;
}

function localName(ctx) {
  return platformLabel(ctx.state.snapshot?.platform ?? ctx.platform, true);
}

// MonHop is always bidirectional: each direction of control is its own switch. Home shows both only
// for the computer in use, since that is the only pairing sharing input right now.
function controlSwitches(ctx, computer, peerName, local) {
  const { actions, sharing } = ctx;
  const syncing = sharing.pending?.kind === "control" || sharing.view?.control?.syncing === true;
  const controls = controlSwitchRows(sharing.view?.control, local, peerName, syncing);
  return el("div", {
    className: "rows control-switches",
    children: controls.map((control) =>
      switchRow(control.label, {
        description: control.hint,
        checked: control.checked,
        disabled: control.disabled,
        focusKey: `control-${computer.fingerprint}-${control.direction}`,
        onChange: (checked) => actions.setControl(computer.fingerprint, control.direction, checked),
      }),
    ),
  });
}

function forgetControls(ctx, computer, name) {
  const { actions, busy, forgetConfirmed } = ctx;
  const confirmed = forgetConfirmed === computer.fingerprint;
  return [
    note(`Fingerprint ${computer.fingerprint.slice(0, 16)}…`),
    rows([
      switchRow(`Forget ${name}`, {
        description: "Removes the pairing. This computer keeps its own identity.",
        checked: confirmed,
        disabled: busy,
        focusKey: `forget-confirm-${computer.fingerprint}`,
        onChange: (checked) => actions.confirmForget(computer.fingerprint, checked),
      }),
    ]),
    el("div", {
      className: "card-actions",
      attrs: { "data-align": "start" },
      children: [
        button("Forget computer", {
          variant: "destructive",
          size: "sm",
          iconName: "trash-2",
          disabled: !confirmed || busy,
          focusKey: `forget-${computer.fingerprint}`,
          onClick: () => actions.forgetComputer(computer.fingerprint),
        }),
      ],
    }),
  ];
}

// --- displays ---------------------------------------------------------------

// The picture is the only place a card names displays, so nothing is listed twice. Home's card for
// the computer in use draws its own bigger one and asks for this to be left out.
function cardArrangement(ctx, computer, key) {
  // A computer with nothing saved still says so: blank space under the header reads as a fault.
  if (computer.setup?.saved !== true)
    return note("No layout yet. Arrange the displays to start sharing.");
  const localPlatform = ctx.state.snapshot?.platform ?? ctx.platform;
  return el("div", {
    className: "computer-viewport",
    children: [
      createDashboardArrangement(computer.setup, {
        local: localName(ctx),
        peer: displayName(computer),
        localPlatform,
        peerPlatform: computer.platform,
        compact: true,
        caption: displaysFreshness(computer.setup),
        motionKey: `${key}-viewport`,
      }),
    ],
  });
}

// A display change MonHop is already settling says so on the card itself. Only a change the user
// has to act on is worth the banner Home puts above everything.
function noticeLine(ctx, computer, inUse) {
  const notice = ctx.sharing.view?.displayNotice;
  if (!inUse || !notice || noticePresentation(notice.kind) !== "inline") return null;
  const copy = displayNoticeCopy(notice.kind, displayName(computer));
  return el("p", {
    className: "notice-line",
    attrs: { "aria-live": "polite" },
    children: [
      el("span", { className: "notice-dot", attrs: { "aria-hidden": "true" } }),
      el("span", { text: copy.body }),
    ],
  });
}

// --- layout history ---------------------------------------------------------

function layoutsDisclosure(ctx, computer, key) {
  const { actions, active, sharing, busy, layoutForget } = ctx;
  const fingerprint = computer.fingerprint;
  const store = computerArrangements(ctx.computerArrangements, fingerprint);
  const entries = layoutRows({
    fingerprint,
    entries: store.items,
    armed: layoutForget,
    isActive: fingerprint === active,
    connected: isConnected(sharing),
    busy,
    pending: store.loading,
  });
  const content = [];
  if (store.error) content.push(note(store.error, "danger"));
  if (!entries.length)
    content.push(
      note(
        store.loading
          ? "Reading the saved layouts…"
          : "MonHop remembers each layout applied with this computer.",
      ),
    );
  else content.push(layoutList(ctx, fingerprint, entries));
  const label = entries.length ? `Layouts (${entries.length})` : "Layouts";
  const accordion = createAccordion(`${key}-layouts`, "computer-layouts", label, ...content);
  // The manager toggles the panel first and announces it afterwards, so reading the list here
  // re-renders the card without the press being lost.
  accordion.addEventListener(ACCORDION_TOGGLE, (event) => {
    if (event.detail.open) void actions.loadComputerArrangements(fingerprint);
  });
  return accordion;
}

// The list is rebuilt on every status poll, so the rows only play their entrance when the set of
// names actually changed; each row follows the one above it.
function layoutList(ctx, fingerprint, entries) {
  const list = rows(entries.map((item, index) => layoutRow(ctx, fingerprint, item, index)));
  const names = JSON.stringify(entries.map((item) => item.entry.name));
  const elapsed = sinceChanged(`${fingerprint}-layout-list`, names);
  if (elapsed < LIST_ENTER_MS) {
    list.dataset.enter = "true";
    // As the play/stop toggle does: the negative delay resumes the entrance, stagger included,
    // so a status poll part way through it does not send every row back to the start.
    list.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
  }
  return list;
}

// One chip strip for a saved layout, shared with the connected editor's list so the two can never
// disagree. "Fits now" sits in a keyed slot of its own, which fades and scales as it comes and goes.
export function layoutChipStrip(key, entry) {
  const chips = layoutChips(entry);
  return el("span", {
    className: "row-leading-chips",
    dataset: { empty: String(chips.marks.length === 0 && !chips.fits) },
    children: [
      ...chips.marks.map((chip) => statusChip(chip)),
      swap(
        `${key}-fits`,
        chips.fits ? statusChip(chips.fits) : el("span", { className: "chip-slot" }),
        chips.fits ? "fits" : "none",
      ),
    ],
  });
}

function layoutRow(ctx, fingerprint, item, index) {
  const { actions } = ctx;
  const { entry, key, armed, disabled, load } = item;
  const node = row({
    title: entry.name,
    detail: `${entry.crossings} crossing${entry.crossings === 1 ? "" : "s"}`,
    leading: layoutChipStrip(`layout-${key}`, entry),
    actions: [
      loadButton(actions, fingerprint, entry, key, load, disabled),
      button(armed ? "Confirm forget" : "Forget", {
        variant: armed ? "destructive" : "ghost",
        size: "sm",
        disabled,
        pressed: armed,
        focusKey: `layout-forget-${key}`,
        onClick: () => actions.pressLayoutForget(fingerprint, entry.name),
      }),
    ],
  });
  node.style.setProperty("--row-index", String(index));
  return node;
}

// Always drawn, so a row never changes shape as the connection comes and goes; the title says
// what is missing while it cannot be pressed.
function loadButton(actions, fingerprint, entry, key, load, disabled) {
  const node = button("Load", {
    variant: "outline",
    size: "sm",
    disabled: disabled || !load.enabled,
    focusKey: `layout-load-${key}`,
    onClick: () => void actions.loadComputerArrangement(fingerprint, entry.name),
  });
  if (load.reason) node.title = load.reason;
  return node;
}
