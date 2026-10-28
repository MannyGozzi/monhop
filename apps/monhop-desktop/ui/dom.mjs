// Small DOM helpers and the shared component vocabulary (button, badge, card, row, icon button with tooltip).
import { copyPlacement } from "./glide-model.mjs";
import { icon } from "./icons.mjs";
import { dimFollow, spinTurn, tweenValue, usableEasing } from "./motion-model.mjs";
import { pairBadgeLabel, pairBadgeParts } from "./pair-badge-model.mjs";

export { icon };

export function el(tag, { className, text: content, attrs, children, dataset } = {}) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (content !== undefined && content !== null) node.textContent = content;
  if (attrs)
    for (const [key, value] of Object.entries(attrs))
      if (value !== null && value !== undefined) node.setAttribute(key, String(value));
  if (dataset)
    for (const [key, value] of Object.entries(dataset))
      if (value !== null && value !== undefined) node.dataset[key] = String(value);
  if (children) node.append(...children.filter(Boolean));
  return node;
}

export function text(value) {
  return document.createTextNode(value);
}

export function clear(node) {
  node.replaceChildren();
}

export function button(
  label,
  {
    variant = "default",
    size,
    disabled = false,
    busy = false,
    focusKey,
    id,
    iconName,
    onClick,
    pressed,
  } = {},
) {
  const node = el("button", { className: "button", attrs: { type: "button" } });
  if (iconName) node.append(icon(iconName));
  if (label) node.append(text(label));
  node.disabled = disabled;
  node.dataset.variant = variant;
  if (size) node.dataset.size = size;
  if (busy) node.setAttribute("aria-busy", "true");
  if (focusKey) node.dataset.focusKey = focusKey;
  if (id) node.id = id;
  if (pressed !== undefined) node.setAttribute("aria-pressed", String(pressed));
  if (onClick) node.addEventListener("click", onClick);
  return node;
}

// `art` replaces the single icon for a button whose glyph is built by its caller (a state toggle
// that draws both of its glyphs, say); everything else about the control stays the same.
export function iconButton({
  id,
  label,
  iconName = "refresh-cw",
  art = null,
  disabled = false,
  busy = false,
  size,
  variant = "outline",
  onClick,
}) {
  const node = button("", { variant, size, disabled, busy, focusKey: id, id, onClick });
  node.classList.add("icon-button");
  node.setAttribute("aria-label", label);
  const glyph = art ?? icon(iconName);
  turnGlyph(glyph, id, busy);
  node.append(glyph);
  return withTooltip(node, `${id}-tooltip`, label);
}

// Each busy glyph's turn, by key, so a rebuilt glyph keeps its phase and an idle one finishes its turn.
const spins = new Map();

// Spins `glyph` while `busy` and for the rest of its turn after; phaseLoops places the CSS loop.
export function turnGlyph(glyph, key, busy) {
  const last = spins.get(key) ?? null;
  const turn = spinTurn(last, { busy, now: performance.now(), period: motionMs("--loop-spin") });
  spins.set(key, turn);
  // A kept glyph whose last loop already finished needs a fresh one; reading the style drops the old.
  if (turn.turning && turn.startedAt !== last?.startedAt && glyph.classList.contains("spin")) {
    glyph.classList.remove("spin");
    void getComputedStyle(glyph).animationName;
  }
  glyph.classList.toggle("spin", turn.turning);
  if (!turn.turning) {
    delete glyph.dataset.spinStart;
    delete glyph.dataset.spinEnd;
    return;
  }
  glyph.dataset.spinStart = String(turn.startedAt);
  glyph.dataset.spinEnd = String(turn.endsAt);
}

// Loops on nodes a render rebuilds run on the document clock, so a rebuild never restarts one: a busy
// glyph from when its work started (turnGlyph), any other spinner or pulse from the clock's origin.
const CLOCK_LOOPS = new Set(["spin", "notice-dot"]);

export function phaseLoops(root = document) {
  if (typeof root.getAnimations !== "function") return;
  const period = motionMs("--loop-spin");
  for (const animation of root.getAnimations()) {
    if (!CLOCK_LOOPS.has(animation.animationName)) continue;
    const { spinStart, spinEnd } = animation.effect?.target?.dataset ?? {};
    const start = spinStart === undefined ? 0 : Number(spinStart);
    if (animation.startTime !== start) animation.startTime = start;
    if (spinStart === undefined || !(period > 0)) continue;
    const end = Number(spinEnd);
    const iterations = Number.isFinite(end) ? Math.max(1, (end - start) / period) : Infinity;
    if (animation.effect.getTiming().iterations !== iterations)
      animation.effect.updateTiming({ iterations });
  }
}

// Shows `label` while `node` is hovered or focused. Escape hides it until the next hover or focus.
export function withTooltip(node, id, label) {
  node.dataset.tooltipDismissed = "false";
  const tooltip = el("span", { className: "tooltip", text: label, attrs: { role: "tooltip", id } });
  node.setAttribute("aria-describedby", id);
  node.append(tooltip);
  const restore = () => {
    node.dataset.tooltipDismissed = "false";
  };
  node.addEventListener("pointerenter", restore);
  node.addEventListener("focus", restore);
  node.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    node.dataset.tooltipDismissed = "true";
    event.preventDefault();
  });
  return node;
}

export function badge(label, tone = "secondary", iconName = null) {
  const node = el("span", { className: "badge", dataset: { tone } });
  if (iconName) node.append(icon(iconName));
  node.append(text(label));
  return node;
}

export function note(value, tone = null) {
  return el("p", { className: "note", text: value, dataset: tone ? { tone } : undefined });
}

export function card({
  title,
  description,
  tone,
  id,
  actions,
  children = [],
  headingActions,
} = {}) {
  const node = el("section", { className: "card", dataset: tone ? { tone } : undefined });
  if (id) node.id = id;
  if (title) {
    const copy = el("div", {
      children: [el("h3", { text: title }), description ? el("p", { text: description }) : null],
    });
    node.append(
      el("div", {
        className: "card-heading",
        children: [
          copy,
          headingActions ? el("div", { className: "row-actions", children: headingActions }) : null,
        ],
      }),
    );
  }
  node.append(...children.filter(Boolean));
  if (actions?.length) node.append(el("div", { className: "card-actions", children: actions }));
  return node;
}

export function rows(items) {
  return el("div", { className: "rows", children: items });
}

// A disabled selectable row keeps its place and its checked mark but takes no press.
export function row({
  title,
  detail,
  leading,
  actions,
  selectable = false,
  checked = null,
  disabled = false,
  onSelect,
  focusKey,
}) {
  const node = el("div", { className: "row" });
  const copy = el("div", {
    className: "row-copy",
    children: [el("strong", { text: title }), detail ? el("span", { text: detail }) : null],
  });
  node.append(leading ? el("div", { className: "row-leading", children: [leading, copy] }) : copy);
  if (actions?.length) node.append(el("div", { className: "row-actions", children: actions }));
  if (selectable) {
    node.dataset.selectable = "true";
    node.setAttribute("role", "radio");
    node.setAttribute("aria-checked", String(checked === true));
    if (disabled) {
      node.setAttribute("aria-disabled", "true");
      return node;
    }
    node.tabIndex = 0;
    if (focusKey) node.dataset.focusKey = focusKey;
    node.addEventListener("click", () => onSelect?.());
    node.addEventListener("keydown", (event) => {
      if (event.key !== "Enter" && event.key !== " ") return;
      event.preventDefault();
      onSelect?.();
    });
  }
  return node;
}

export function radioMark() {
  return el("span", { className: "radio-mark", attrs: { "aria-hidden": "true" } });
}

// The header pill and every computer card show one status the same way.
export function statusChip(status) {
  return el("span", {
    className: "status-pill",
    dataset: { tone: status.tone },
    children: [
      el("span", { className: "status-dot", attrs: { "aria-hidden": "true" } }),
      el("span", { text: status.label }),
    ],
  });
}

export function facts(pairs) {
  const list = el("dl", { className: "facts" });
  for (const [term, definition] of pairs)
    list.append(el("dt", { text: term }), el("dd", { text: definition }));
  return list;
}

// The match badge both computers draw for a pairing: one color and three symbols, "lg" on the
// pairing screen and "sm" on a computer card.
export function pairBadge(match, size = "sm") {
  const { color, symbols } = pairBadgeParts(match);
  const node = el("span", {
    className: "pair-badge",
    dataset: { color: color.key, size },
    attrs: { role: "img", "aria-label": pairBadgeLabel(match) },
  });
  symbols.forEach((symbol, index) => {
    const glyph = el("span", { className: "pair-badge-symbol", children: [icon(symbol.icon)] });
    glyph.style.setProperty("--symbol-index", String(index));
    node.append(glyph);
  });
  return node;
}

// Enter animations key on a card's tone and title, so a status poll that changes nothing draws nothing.
const stateSignatures = new Map();
const BUSY_TONES = new Set(["checking", "syncing", "stopping"]);

// A state on its way sends rings out from its glyph and runs a light along the card's lower edge.
export function stateCard({ tone, iconName, title, detail, actions = [], id }) {
  const busy = BUSY_TONES.has(tone);
  const ripples = busy
    ? [el("span", { className: "state-ripple" }), el("span", { className: "state-ripple" })]
    : [];
  const glyph = el("span", {
    className: "state-glyph",
    dataset: { tone },
    attrs: { "aria-hidden": "true" },
    children: [...ripples, icon(iconName ?? "link", 18)],
  });
  const node = el("section", {
    className: "card state-card",
    dataset: { busy: String(busy) },
    children: [
      glyph,
      el("div", {
        className: "state-copy",
        children: [el("strong", { text: title }), detail ? el("p", { text: detail }) : null],
      }),
      el("div", { className: "state-actions", children: actions }),
    ],
  });
  if (id) {
    node.id = id;
    const signature = `${tone}|${title}`;
    node.dataset.enter = String(stateSignatures.get(id) !== signature);
    stateSignatures.set(id, signature);
  }
  return node;
}

// Durations and easings live in styles.css; JS motion reads them there so both run on one system.
// Cached: they never change at runtime, and reading one mid-render would force a style recalc.
const motionTokens = new Map();

export function motionToken(name) {
  if (!motionTokens.has(name)) {
    const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    if (!value) return "";
    motionTokens.set(name, value);
  }
  return motionTokens.get(name);
}

export function motionMs(name) {
  return Number.parseFloat(motionToken(name)) || 0;
}

const easings = new Map();
const parsesAsEasing = (value) => CSS.supports("transition-timing-function", value);

// Every Web Animations easing comes through here: an easing token the engine cannot parse falls
// back to --ease-out, where animate() would throw.
export function motionEase(name = "--ease-out") {
  if (easings.has(name)) return easings.get(name);
  const tokens = [motionToken(name), motionToken("--ease-out")];
  const easing = usableEasing(tokens, parsesAsEasing);
  if (tokens[0]) easings.set(name, easing);
  return easing;
}

// Motion helpers. Each keyed slot remembers what it showed and when that last changed, so a
// change animates once, keeps playing through the re-renders that happen meanwhile, and is
// skipped under reduced motion or while the page is hidden.
const motionMemory = new Map();
const enterMs = () => motionMs("--motion-base");
const exitMs = () => motionMs("--motion-fast");
// Enters rise into place; exits drift half as far upward, eased in and out so the first frame is no cut.
const rise = () => motionToken("--motion-rise") || "0px";
const shrink = () => motionToken("--scale-enter") || "1";
const scaled = (scale) => (scale ? ` scale(${shrink()})` : "");
const enterFrames = (scale = true) => [
  { opacity: 0, transform: `translateY(${rise()})${scaled(scale)}` },
  { opacity: 1, transform: "none" },
];
const exitTo = (scale = true) => ({
  opacity: 0,
  transform: `translateY(calc(${rise()} * -.5))${scaled(scale)}`,
});
const exitFrames = (scale) => [{ opacity: 1, transform: "none" }, exitTo(scale)];
const EXIT_EASE = "--ease-in-out";

// The one reduced-motion query all UI motion reads.
export const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

// UI motion runs only when it is wanted and on screen.
// A block built fresh, such as a Settings page shown again, lands as it is: everything built inside
// records its current state without animating, so nothing replays what changed while it was away.
let settling = false;

export function motionEnabled() {
  return !settling && !reducedMotion.matches && !document.hidden;
}

export function settled(build) {
  settling = true;
  try {
    return build();
  } finally {
    settling = false;
  }
}

// Where each container that turned inert sent the focus it held, so focus never drops to <body>.
const focusHandoffs = new WeakMap();

// Call before `container` turns inert: focus inside moves to `target` now, and a render that finds
// its old focus inside later sends it there too.
export function handOffFocus(container, target) {
  focusHandoffs.set(container, target);
  if (container.contains(document.activeElement)) target?.focus({ preventScroll: true });
}

// Where focus a render saw on `node` belongs once an inert container around it handed it off.
export function handedOffFocus(node) {
  for (let at = node.closest("[inert]"); at; at = at.parentElement?.closest("[inert]") ?? null)
    if (focusHandoffs.has(at)) return focusHandoffs.get(at);
  return null;
}

// Plays `keyframes` as if they started `elapsed` ms ago, so a re-render continues the motion
// instead of restarting it. The animation is cancelled once it has finished.
function play(node, keyframes, duration, elapsed, after, ease = "--ease-out") {
  const animation = node.animate(keyframes, { duration, easing: motionEase(ease), fill: "both" });
  animation.currentTime = Math.min(Math.max(elapsed, 0), duration);
  void settle(animation, after);
  return animation;
}

// A finished fill-both animation is cancelled so it stops counting as running.
export async function settle(animation, after) {
  try {
    await animation.finished;
  } catch {
    return;
  }
  animation.cancel();
  after?.();
}

// The same animation on `node`, at the same point in its timing.
export function copyAnimation(animation, node) {
  const copy = node.animate(animation.effect.getKeyframes(), animation.effect.getTiming());
  const placement = copyPlacement(animation);
  if ("startTime" in placement) copy.startTime = placement.startTime;
  else copy.currentTime = placement.currentTime;
  return copy;
}

// What a running tween of one number draws now; animations outlive their detached targets.
function tweenNow(tween) {
  if (tween?.animation?.playState !== "running") return null;
  return tweenValue(tween.from, tween.to, tween.animation.effect.getComputedTiming().progress);
}

// Controls a render rebuilt cannot transition their own disabled look, so each keyed control's drawn
// opacity is read before the render (captureDims) and its new node fades from there (followDims).
const DIMMABLE = "button[data-focus-key], button[id], input[data-focus-key], input[id]";
const dimFades = new Map();

function dimKey(node) {
  return node.dataset.focusKey || node.id || null;
}

function fadesOpacity(animation) {
  return animation.effect?.getKeyframes().some((frame) => "opacity" in frame) ?? false;
}

// A slider dims its whole row, every other control itself.
function dimTarget(node) {
  return node.closest(".slider-row") ?? node;
}

// What sets a control's dimmed look: its classes and its disabled and busy states.
function dimLook(node) {
  const { className, disabled } = node;
  return [
    className,
    disabled,
    node.getAttribute("aria-disabled"),
    node.getAttribute("aria-busy"),
  ].join("|");
}

export function captureDims(root) {
  const drawn = new Map();
  if (!motionEnabled()) return drawn;
  for (const node of root.querySelectorAll(DIMMABLE)) {
    const key = dimKey(node);
    if (key && node.getClientRects().length)
      drawn.set(key, {
        node,
        look: dimLook(node),
        opacity: Number(getComputedStyle(dimTarget(node)).opacity),
      });
  }
  return drawn;
}

export function followDims(root, drawn) {
  if (!drawn.size || !motionEnabled()) return;
  for (const node of root.querySelectorAll(DIMMABLE)) {
    const key = dimKey(node);
    const last = drawn.get(key);
    // A kept node transitions on its own.
    if (!last || last.node === node) continue;
    const running = dimFades.get(key) ?? null;
    // Built as the one it replaces and not mid-fade, it rests as that one did. Asked before any
    // style read, since the first one in a render costs a style and layout pass.
    if (last.look === dimLook(node) && tweenNow(running) === null) {
      dimFades.delete(key);
      continue;
    }
    const target = dimTarget(node);
    const resting = Number(getComputedStyle(target).opacity);
    const follow = dimFollow({
      drawn: last.opacity,
      resting,
      running: tweenNow(running) === null ? null : running.to,
    });
    if (follow === null && !running) continue;
    // One already fading in some other way keeps that motion.
    if (target.getAnimations().some(fadesOpacity)) continue;
    if (follow === "carry") {
      dimFades.set(key, { ...running, animation: copyAnimation(running.animation, target) });
      continue;
    }
    dimFades.delete(key);
    if (follow !== "fade") continue;
    const animation = target.animate(
      { opacity: [last.opacity, resting] },
      { duration: motionMs("--motion-fast"), easing: motionEase() },
    );
    dimFades.set(key, { animation, from: last.opacity, to: resting });
  }
}

// A determinate bar whose fill scales on the compositor. A render rebuilds the bar, so the fill
// glides from what the last one drew, or carries its glide on when the value did not change.
const progressFills = new Map();

export function progressBar(key, fraction) {
  const value = Math.min(Math.max(fraction, 0), 1);
  const fill = el("div", { className: "progress-bar-fill" });
  fill.style.setProperty("--progress", String(value));
  const last = progressFills.get(key) ?? null;
  const shown = tweenNow(last) ?? last?.to ?? null;
  let tween = null;
  if (motionEnabled() && last && last.to === value && tweenNow(last) !== null)
    tween = { ...last, animation: copyAnimation(last.animation, fill) };
  else if (motionEnabled() && shown !== null && shown !== value) {
    const animation = fill.animate(
      { transform: [`scaleX(${shown})`, `scaleX(${value})`] },
      { duration: motionMs("--motion-slow"), easing: motionEase() },
    );
    tween = { animation, from: shown, to: value };
  }
  progressFills.set(key, tween ?? { animation: null, from: value, to: value });
  return el("div", { className: "progress-bar", children: [fill] });
}

// A visual copy of a node that left: never focusable, never announced, never a duplicate id.
export function ghostOf(node) {
  const ghost = node.cloneNode(true);
  // A copy cannot carry a nested fade's running state, so what was leaving inside it goes now.
  for (const nested of ghost.querySelectorAll("[data-exit]")) nested.remove();
  ghost.dataset.exit = "true";
  ghost.setAttribute("aria-hidden", "true");
  ghost.inert = true;
  for (const item of [ghost, ...ghost.querySelectorAll("[id], [data-focus-key]")]) {
    item.removeAttribute("id");
    delete item.dataset.focusKey;
  }
  return ghost;
}

// A control whose state changed cross-fades in place. Width stays natural so text never drives
// a layout animation. `scale: false` keeps a large block (a whole page) at its size as it fades.
export function swap(key, node, signature, { block = false, scale = true } = {}) {
  const now = performance.now();
  const entry = motionMemory.get(key) ?? { signature, changedAt: -Infinity };
  // Every state still fading out keeps its own timeline; one that was still fading in when it was
  // replaced leaves from where it was drawn.
  const leavers = (entry.leavers ?? []).filter((leaver) => now - leaver.at < exitMs());
  if (entry.signature !== signature) {
    entry.signature = signature;
    entry.changedAt = settling ? -Infinity : now;
    if (entry.ghost && !settling)
      leavers.push({ node: entry.ghost, from: drawnMidEnter(entry.live), at: now });
  }
  entry.leavers = leavers;
  entry.ghost = node.cloneNode(true);
  entry.live = null;
  motionMemory.set(key, entry);
  const wrapper = el(block ? "div" : "span", { className: "swap", children: [node] });
  if (block) wrapper.dataset.block = "true";
  const elapsed = now - entry.changedAt;
  if (elapsed >= enterMs() || !motionEnabled()) return wrapper;
  // Kept only while it fades in, the one stretch a change could catch it mid-way.
  entry.live = { node, enter: play(node, enterFrames(scale), enterMs(), elapsed) };
  for (const leaver of leavers) {
    const ghost = ghostOf(leaver.node);
    wrapper.append(ghost);
    const frames = leaver.from ? [leaver.from, exitTo(scale)] : exitFrames(scale);
    play(ghost, frames, exitMs(), now - leaver.at, () => ghost.remove(), EXIT_EASE);
  }
  return wrapper;
}

// Read only while its own enter runs: asking the node for its animations would force a layout.
function drawnMidEnter(live) {
  if (!live?.node.isConnected || live.enter.playState !== "running") return null;
  const { opacity, transform } = getComputedStyle(live.node);
  return { opacity, transform };
}

// A list's one selection indicator moves into the selected item and slides over from wherever it was
// drawn, so a change mid-slide carries on from there.
const indicatorGlides = new WeakMap();

export function glideIndicator(indicator, target) {
  if (!target) {
    indicator.hidden = true;
    return;
  }
  indicator.hidden = false;
  if (indicator.parentElement === target) return;
  const from = indicator.getClientRects().length ? indicator.getBoundingClientRect() : null;
  indicatorGlides.get(indicator)?.cancel();
  indicatorGlides.delete(indicator);
  target.prepend(indicator);
  if (!from || !motionEnabled()) return;
  const to = indicator.getBoundingClientRect();
  if (!to.width || !to.height) return;
  const transform = `translate(${from.left - to.left}px, ${from.top - to.top}px) scale(${from.width / to.width}, ${from.height / to.height})`;
  const animation = indicator.animate([{ transform }, { transform: "none" }], {
    duration: motionMs("--motion-slow"),
    easing: motionEase("--ease-glide"),
  });
  indicatorGlides.set(indicator, animation);
}

// How long ago a keyed value last changed (Infinity before any change), for a node the next render
// rebuilds from scratch: a CSS animation delayed by minus this continues instead of replaying.
export function changedAgo(key, signature) {
  const now = performance.now();
  const entry = motionMemory.get(key) ?? { signature, changedAt: -Infinity };
  if (entry.signature !== signature) {
    entry.signature = signature;
    entry.changedAt = settling ? -Infinity : now;
  }
  motionMemory.set(key, entry);
  return now - entry.changedAt;
}

// changedAgo, or Infinity when motion is off, meaning nothing to play.
export function sinceChanged(key, signature) {
  const elapsed = changedAgo(key, signature);
  return motionEnabled() ? elapsed : Infinity;
}

// A label that switches between `labels` without changing its control's width, so nothing beside the
// control jumps: every label shares one cell, only `current` shows, and a change cross-fades.
export function stableLabel(key, labels, current) {
  const node = el("span", {
    className: "stable-label",
    children: labels.map((label, index) =>
      el("span", {
        text: label,
        dataset: { current: String(index === current) },
        attrs: { "aria-hidden": index === current ? null : "true" },
      }),
    ),
  });
  const elapsed = sinceChanged(`label-${key}`, current);
  if (elapsed < motionMs("--motion-base")) {
    node.dataset.enter = "true";
    node.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
  }
  return node;
}

// A live label cross-fades in place: the new text rises in while a copy of the old one fades out
// over the spot it held, each from what was drawn the moment the label changed, so a change landing
// mid-fade carries on from there. The node itself only ever holds the current text.
const labelEnters = new WeakMap();

export function setLabel(node, label) {
  if (node.textContent === label) return;
  labelEnters.get(node)?.cancel();
  labelEnters.delete(node);
  const parent = node.parentElement;
  const motion = Boolean(parent) && motionEnabled();
  const ghost = motion && node.textContent ? labelGhost(node, parent) : null;
  node.textContent = label;
  if (!motion) return;
  if (ghost) fitLabelGhost(ghost, parent);
  labelEnters.set(node, play(node, enterFrames(), enterMs(), 0));
}

// The old label as drawn now, laid over the spot it holds and fading out; none when it is not on screen.
function labelGhost(node, parent) {
  parent.dataset.labelFade = "true";
  if (node.offsetParent !== parent) {
    dropLabelGhost(parent, null);
    return null;
  }
  const drawn = getComputedStyle(node);
  const from = { opacity: drawn.opacity, transform: drawn.transform };
  const ghost = ghostOf(node);
  ghost.dataset.labelGhost = "true";
  ghost.style.left = `${node.offsetLeft}px`;
  ghost.style.top = `${node.offsetTop}px`;
  // No wider than the label was, so a truncated one stays truncated; offsetWidth rounds, hence the pixel.
  ghost.style.maxWidth = `${node.offsetWidth + 1}px`;
  parent.append(ghost);
  play(ghost, [from, exitTo()], exitMs(), 0, () => dropLabelGhost(parent, ghost), EXIT_EASE);
  return ghost;
}

// A parent that pulled in to fit a shorter label clips the copy at its new content edge.
function fitLabelGhost(ghost, parent) {
  const edge = parent.clientWidth - Number.parseFloat(getComputedStyle(parent).paddingRight);
  const room = Math.max(edge - ghost.offsetLeft, 0);
  if (room < ghost.offsetWidth) ghost.style.maxWidth = `${room}px`;
}

function dropLabelGhost(parent, ghost) {
  ghost?.remove();
  if (!parent.querySelector(":scope > [data-label-ghost]")) delete parent.dataset.labelFade;
}

export function platformIcon(platform) {
  return platform === "macos" ? "app-window-mac" : "app-window";
}

export function platformGlyph(platform) {
  const svg = icon(platformIcon(platform), 28);
  svg.classList.add("platform-glyph");
  svg.dataset.platform = platform;
  return svg;
}

// The button carries the state; the surrounding label makes the whole row the click target.
export function switchRow(
  label,
  { checked, description, disabled = false, focusKey, id, onChange },
) {
  const control = el("button", {
    className: "switch",
    attrs: { type: "button", role: "switch", "aria-checked": String(checked === true) },
    children: [el("span", { className: "switch-thumb", attrs: { "aria-hidden": "true" } })],
  });
  control.disabled = disabled;
  if (focusKey) control.dataset.focusKey = focusKey;
  if (id) control.id = id;
  // The row is rebuilt on every state push, so a flip springs across from the old side here.
  const motionKey = focusKey ?? id;
  if (motionKey) {
    const elapsed = sinceChanged(`switch-${motionKey}`, checked === true);
    if (elapsed < motionMs("--motion-spring")) {
      control.dataset.enter = "true";
      control.style.setProperty("--motion-delay", `${-Math.round(elapsed)}ms`);
    }
  }
  control.addEventListener("click", () => onChange?.(checked !== true));
  const copy = el("span", {
    className: "switch-copy",
    children: [
      el("span", { text: label }),
      description ? el("small", { text: description }) : null,
    ],
  });
  return el("label", { className: "switch-row", children: [copy, control] });
}

// A range input reports every drag step through onInput and the settled value through onChange. The
// fill grows from `origin`, ticked when inside the range, and `ends` names the two ends under the track.
// With `glide`, a value that arrives without a drag eases there from what the last render drew.
export function sliderRow(
  label,
  {
    value,
    min,
    max,
    step = 1,
    origin = min,
    ends = null,
    format = String,
    valueText = null,
    ariaLabel = null,
    description,
    disabled = false,
    glide = false,
    focusKey,
    id,
    onInput,
    onChange,
    onDragStart,
    onDragEnd,
  },
) {
  const input = el("input", {
    className: "slider",
    attrs: {
      type: "range",
      min: String(min),
      max: String(max),
      step: String(step),
      "aria-label": ariaLabel,
    },
  });
  input.disabled = disabled;
  if (focusKey) input.dataset.focusKey = focusKey;
  if (id) input.id = id;
  const readout = el("output", { className: "slider-value" });
  const at = (number) => `${((number - min) / (max - min)) * 100}%`;
  // The track fills between the origin and `shown`; the words name the step nearest it.
  const paint = (shown) => {
    input.style.setProperty("--slider-from", at(Math.min(shown, origin)));
    input.style.setProperty("--slider-fill", at(Math.max(shown, origin)));
    const nearest = min + Math.round((shown - min) / step) * step;
    readout.textContent = format(nearest);
    if (valueText) input.setAttribute("aria-valuetext", valueText(nearest));
  };
  // Only a glide draws between steps.
  const draw = (shown, between = false) => {
    input.step = between ? "any" : String(step);
    input.value = String(shown);
    paint(shown);
  };
  const key = glide ? (focusKey ?? id ?? null) : null;
  if (key) glideSlider(key, value, draw);
  else draw(value);
  input.addEventListener("input", () => {
    const shown = Number(input.value);
    if (key) holdSlider(key, shown, draw);
    paint(shown);
    onInput?.(shown);
  });
  input.addEventListener("change", () => onChange?.(Number(input.value)));
  if (key)
    for (const event of ["pointerdown", "keydown"])
      input.addEventListener(event, () => landSlider(key));
  if (onDragStart) input.addEventListener("pointerdown", () => onDragStart());
  // A release fires change after pointerup, so a render the drag held waits for it.
  if (onDragEnd)
    for (const event of ["pointerup", "pointercancel"])
      input.addEventListener(event, () => setTimeout(onDragEnd));
  const tick =
    origin > min && origin < max
      ? el("span", { className: "slider-tick", attrs: { "aria-hidden": "true" } })
      : null;
  tick?.style.setProperty("--slider-origin", String((origin - min) / (max - min)));
  const scale = ends
    ? el("span", {
        className: "slider-ends",
        attrs: { "aria-hidden": "true" },
        children: ends.map((end) => el("small", { text: end })),
      })
    : null;
  const copy = el("span", {
    className: "switch-copy",
    children: [
      el("span", { text: label }),
      description ? el("small", { text: description }) : null,
    ],
  });
  return el("label", {
    className: "slider-row",
    children: [
      copy,
      el("span", { className: "slider-control", children: [tick, input, readout, scale] }),
    ],
  });
}

// Each keyed slider's glide toward a value it was given without a drag, carried across the renders that
// rebuild its input: { animation, from, to, draw }, with no animation once it rests.
const sliderGlides = new Map();

// A new value glides from what is drawn now; one the glide already heads for carries it on.
function glideSlider(key, value, draw) {
  const last = sliderGlides.get(key) ?? null;
  const midway = tweenNow(last);
  if (motionEnabled() && midway !== null && last.to === value) {
    last.draw = draw;
    draw(midway, true);
    return;
  }
  last?.animation?.cancel();
  const drawn = midway ?? last?.to ?? value;
  if (!motionEnabled() || drawn === value) {
    sliderGlides.set(key, { animation: null, from: value, to: value, draw });
    draw(value);
    return;
  }
  const timing = { duration: motionMs("--motion-slow"), easing: motionEase("--ease-glide") };
  const animation = new Animation(new KeyframeEffect(null, null, timing), document.timeline);
  const tween = { animation, from: drawn, to: value, draw };
  sliderGlides.set(key, tween);
  animation.play();
  draw(drawn, true);
  const frame = () => {
    if (sliderGlides.get(key) !== tween) return;
    const shown = tweenNow(tween);
    if (shown === null) tween.draw(tween.to);
    else {
      tween.draw(shown, true);
      requestAnimationFrame(frame);
    }
  };
  requestAnimationFrame(frame);
}

// A drag or key press starts from the value the slider stands for, so a glide still running lands first.
function landSlider(key) {
  const tween = sliderGlides.get(key);
  if (tweenNow(tween) === null) return;
  tween.animation.cancel();
  tween.draw(tween.to);
}

// What the user drew is where the slider rests.
function holdSlider(key, shown, draw) {
  sliderGlides.get(key)?.animation?.cancel();
  sliderGlides.set(key, { animation: null, from: shown, to: shown, draw });
}

// The Copy/Copied button (state-driven label and icon) plus its aria-live feedback line, shared
// by every copy-to-clipboard control. Callers place `button` and `line` into their own layout.
export function copyFeedbackControls(feedback, { disabled, focusKey, onClick }) {
  return {
    button: button(feedback.state === "success" ? "Copied" : "Copy", {
      variant: "outline",
      size: "sm",
      iconName: feedback.state === "success" ? "check" : "copy",
      disabled,
      focusKey,
      onClick,
    }),
    line: el("p", {
      className: "copy-feedback",
      text: feedback.message,
      attrs: { "aria-live": "polite" },
      dataset: { state: feedback.state },
    }),
  };
}

export function nativeError(error) {
  return error instanceof Error && error.message ? error.message : String(error);
}
