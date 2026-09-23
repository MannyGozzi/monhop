// Small DOM helpers and the shared component vocabulary (button, badge, card, row, icon button with tooltip).
import { icon } from "./icons.mjs";
import { usableEasing } from "./motion-model.mjs";

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
  node.dataset.tooltipDismissed = "false";
  node.setAttribute("aria-label", label);
  const glyph = art ?? icon(iconName);
  if (busy) glyph.classList.add("spin");
  const tooltip = el("span", {
    className: "tooltip",
    text: label,
    attrs: { role: "tooltip", id: `${id}-tooltip` },
  });
  node.setAttribute("aria-describedby", tooltip.id);
  node.append(glyph, tooltip);
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

export function row({
  title,
  detail,
  leading,
  actions,
  selectable = false,
  checked = null,
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

export function fingerprintBlock(label, value) {
  return el("div", {
    className: "fingerprint",
    children: [el("span", { text: label }), el("code", { text: value })],
  });
}

// Enter animations key on a card's tone and title, so a status poll that changes nothing draws nothing.
const stateSignatures = new Map();

export function stateCard({ tone, iconName, title, detail, actions = [], id }) {
  const glyph = el("span", {
    className: "state-glyph",
    dataset: { tone },
    attrs: { "aria-hidden": "true" },
  });
  glyph.append(
    tone === "checking" || tone === "syncing" || tone === "stopping"
      ? el("span", { className: "spinner" })
      : icon(iconName ?? "link", 18),
  );
  const node = el("section", {
    className: "card state-card",
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
const ENTER_FRAMES = [
  { opacity: 0, transform: "translateY(3px) scale(.97)" },
  { opacity: 1, transform: "none" },
];
const EXIT_FRAMES = [
  { opacity: 1, transform: "none" },
  { opacity: 0, transform: "translateY(-2px) scale(.97)" },
];

// The one reduced-motion query all UI motion reads.
export const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

// UI motion runs only when it is wanted and on screen.
export function motionEnabled() {
  return !reducedMotion.matches && !document.hidden;
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
function play(node, keyframes, duration, elapsed, after) {
  const animation = node.animate(keyframes, { duration, easing: motionEase(), fill: "both" });
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

// A visual copy of a node that left: never focusable, never announced, never a duplicate id.
export function ghostOf(node) {
  const ghost = node.cloneNode(true);
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
// a layout animation.
export function swap(key, node, signature, { block = false } = {}) {
  const now = performance.now();
  const entry = motionMemory.get(key) ?? { signature, changedAt: -Infinity };
  if (entry.signature !== signature) {
    entry.signature = signature;
    entry.changedAt = now;
    entry.leaving = entry.ghost;
  }
  entry.ghost = node.cloneNode(true);
  motionMemory.set(key, entry);
  const wrapper = el(block ? "div" : "span", { className: "swap", children: [node] });
  if (block) wrapper.dataset.block = "true";
  const elapsed = now - entry.changedAt;
  if (elapsed >= enterMs() || !motionEnabled()) return wrapper;
  play(node, ENTER_FRAMES, enterMs(), elapsed);
  if (entry.leaving && elapsed < exitMs()) {
    const ghost = ghostOf(entry.leaving);
    wrapper.append(ghost);
    play(ghost, EXIT_FRAMES, exitMs(), elapsed, () => ghost.remove());
  }
  return wrapper;
}

// How long ago a keyed value last changed (Infinity before any change), for a node the next render
// rebuilds from scratch: a CSS animation delayed by minus this continues instead of replaying.
export function changedAgo(key, signature) {
  const now = performance.now();
  const entry = motionMemory.get(key) ?? { signature, changedAt: -Infinity };
  if (entry.signature !== signature) {
    entry.signature = signature;
    entry.changedAt = now;
  }
  motionMemory.set(key, entry);
  return now - entry.changedAt;
}

// changedAgo, or Infinity when motion is off, meaning nothing to play.
export function sinceChanged(key, signature) {
  const elapsed = changedAgo(key, signature);
  return motionEnabled() ? elapsed : Infinity;
}

// A live label fades in place. Its container keeps normal layout rather than animating width.
export function setLabel(node, label) {
  if (node.textContent === label) return;
  node.textContent = label;
  if (!motionEnabled()) return;
  play(node, ENTER_FRAMES, enterMs(), 0);
}

export function platformGlyph(platform) {
  const svg = icon(platform === "macos" ? "app-window-mac" : "app-window", 28);
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

// A range input reports every drag step through onInput and the settled value through onChange.
export function sliderRow(
  label,
  {
    value,
    min,
    max,
    step = 1,
    format = String,
    description,
    disabled = false,
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
    attrs: { type: "range", min: String(min), max: String(max), step: String(step) },
  });
  input.value = String(value);
  input.disabled = disabled;
  if (focusKey) input.dataset.focusKey = focusKey;
  if (id) input.id = id;
  const readout = el("output", { className: "slider-value", text: format(value) });
  // The track paints its filled part up to --slider-fill.
  const fill = () =>
    input.style.setProperty(
      "--slider-fill",
      `${((Number(input.value) - min) / (max - min)) * 100}%`,
    );
  fill();
  input.addEventListener("input", () => {
    fill();
    readout.textContent = format(Number(input.value));
    onInput?.(Number(input.value));
  });
  input.addEventListener("change", () => onChange?.(Number(input.value)));
  if (onDragStart) input.addEventListener("pointerdown", () => onDragStart());
  if (onDragEnd)
    for (const event of ["pointerup", "pointercancel"])
      input.addEventListener(event, () => onDragEnd());
  const copy = el("span", {
    className: "switch-copy",
    children: [
      el("span", { text: label }),
      description ? el("small", { text: description }) : null,
    ],
  });
  return el("label", {
    className: "slider-row",
    children: [copy, el("span", { className: "slider-control", children: [input, readout] })],
  });
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

export function textarea({
  id,
  value = "",
  placeholder,
  readOnly = false,
  disabled = false,
  rows: lineCount = 3,
  ariaLabel,
  maxLength,
  onInput,
}) {
  const node = el("textarea", {
    className: "textarea",
    attrs: { id, rows: lineCount, placeholder, "aria-label": ariaLabel, maxlength: maxLength },
  });
  node.value = value;
  node.readOnly = readOnly;
  node.disabled = disabled;
  node.spellcheck = false;
  if (onInput) node.addEventListener("input", () => onInput(node));
  return node;
}

export function nativeError(error) {
  return error instanceof Error && error.message ? error.message : String(error);
}
