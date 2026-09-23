import {
  changedAgo,
  ghostOf,
  handOffFocus,
  motionEase,
  motionEnabled,
  motionMs,
  motionToken,
  reducedMotion,
} from "./dom.mjs";
import {
  contentMove,
  copyPlacement,
  gapMargin,
  gapShare,
  moved,
  panelNeedsChange,
  rebuiltDisclosure,
  rebuiltPresence,
  settledBox,
} from "./glide-model.mjs";
import { icon } from "./icons.mjs";

let sequence = 0;
// The running glide of each element it moves; a new glide on that element replaces it from where it is.
const glides = new Map();
// Each disclosure key's last asked-for state and current panel. Renders rebuild disclosures, and the
// fresh node starts from here, so it carries on a running glide instead of landing it.
const disclosures = new Map();
// Each presence key's last panel, the node it drew live and the node that left, which a leaving
// panel keeps drawing as a ghost.
const presences = new Map();
// Presence panels a render built that are opened or closed once they are in the page.
let unplaced = [];
// Panels drawing what left, which go once they have closed.
const ghostPanels = new WeakSet();
// Each watched panel content's last observed box. Content that changes size under a panel moves the
// panel with it, whatever changed it, so no call site has to remember to.
const contentBoxes = new WeakMap();
const watched = new WeakSet();
const contentObserver =
  typeof ResizeObserver === "function" ? new ResizeObserver(followContent) : null;
// Contents sitting out the observer for a frame, see benchAround.
const benched = new Set();
// Running chevron turns. A rebuilt chevron starts in its final state and detaching the old one
// cancels its CSS transitions, so only a script animation can be carried over.
const turns = new Set();
reducedMotion.addEventListener("change", () => {
  for (const [node, entry] of glides) endGlide(node, entry.settle);
  for (const turn of turns) turn.finish();
});

// Fired on the accordion element when the user toggles it, never when a render reopens it.
export const ACCORDION_TOGGLE = "accordion-toggle";

// Glides a clipping panel's height to its one child's, or to 0, with the child fading in step. The
// state it has or heads to is a no-op, so renders never restart a glide; the other reverses it.
export function setPanelOpen(panel, open, { instant = false } = {}) {
  watchContent(panel);
  const target = open === true;
  const running = glides.get(panel);
  const glidingTo = running ? running.target : null;
  if (!panelNeedsChange({ glidingTo, hidden: panel.hidden, open: target, instant })) return;
  const settle = () => land(panel, target);
  if (instant || !canGlide(panel)) {
    endGlide(panel, settle);
    return;
  }
  const gap = gapOf(panel);
  const from = running || !target ? panel.getBoundingClientRect().height : 0;
  const margin = [running ? bottomMargin(panel) : gapMargin(gap, !target), gapMargin(gap, target)];
  const opacity = running ? contentOpacity(panel) : null;
  stopGlide(panel);
  panel.hidden = false;
  const to = target ? contentHeight(panel) : 0;
  if (!moved(to, from) && !moved(...margin)) {
    settle();
    return;
  }
  const fade = fadeContent(panel.firstElementChild, target, opacity);
  glideHeight(panel, from, to, { target, fade, settle, gap, margin });
}

// Where a panel's glide lands: shown or hidden, and a closed ghost gone from the page.
function land(panel, open) {
  panel.hidden = !open;
  if (open || !ghostPanels.has(panel)) return;
  panel.remove();
  unwatch(panel.firstElementChild);
}

// A block that comes and goes in place: a disclosure without its trigger.
export function revealPanel(content) {
  const panel = document.createElement("div");
  panel.className = "glide-panel";
  panel.hidden = true;
  panel.inert = true;
  panel.append(content);
  return panel;
}

// Shows or hides a reveal panel. On its way out it turns inert at once, so focus inside first moves
// to `focusTarget`, as a closing disclosure hands it to its trigger.
export function setRevealOpen(panel, open, { focusTarget = null } = {}) {
  if (!open) handOffFocus(panel, focusTarget);
  panel.inert = !open;
  setPanelOpen(panel, open);
}

// A keyed block that comes and goes across renders: its height glides between 0 and its content's
// while the content fades, and the gap it takes in a flex column glides with it. A render mid-glide
// carries the glide on; asking for the other state reverses it from where it is.
export function presence(key, node) {
  const present = Boolean(node);
  const last = presences.get(key) ?? null;
  const drawn = last?.panel ?? null;
  const motion = motionEnabled();
  const start = rebuiltPresence({
    present,
    motion,
    last: last && {
      drawn: drawn !== null,
      hidden: drawn?.hidden === true,
      gliding: glides.has(drawn),
    },
  });
  const left = present ? null : (last?.node ?? last?.left ?? null);
  if (!start || (start.ghost && !left)) {
    presences.set(key, { panel: null, node: null, left: null });
    return null;
  }
  const panel = document.createElement("div");
  panel.className = "glide-panel";
  panel.hidden = start.hidden;
  panel.append(start.ghost ? ghostOf(left) : node);
  if (start.ghost) ghostPanels.add(panel);
  if (start.glide) transplant(drawn, panel);
  if (drawn) handOverContent(drawn, panel);
  presences.set(key, { panel, node, left });
  if (motion) placeLater(panel, present);
  return panel;
}

// Opens or closes each presence panel once it is in the page and can be measured, before it paints.
function placeLater(panel, open) {
  if (!unplaced.length) queueMicrotask(placePanels);
  unplaced.push({ panel, open });
}

// One on a page that is not shown cannot be measured, so it lands at once.
function placePanels() {
  const batch = unplaced;
  unplaced = [];
  for (const { panel, open } of batch)
    if (panel.isConnected)
      setPanelOpen(panel, open, { instant: !panel.parentElement.getClientRects().length });
}

// Runs after layout and before paint, so a panel drawn at its old height glides from there. An
// opening glide heads for the new height instead of snapping to it when it lands.
function followContent(entries) {
  const view = `${window.innerWidth}x${window.innerHeight}`;
  // Deepest first: an inner panel that starts gliding benches the contents around it, whose entries
  // in this same batch are then skipped instead of starting a redundant glide of their own.
  const ordered = entries
    .map((entry) => ({ entry, depth: depthOf(entry.target) }))
    .toSorted((a, b) => b.depth - a.depth);
  for (const { entry } of ordered) {
    const { target: content, borderBoxSize, contentRect } = entry;
    if (benched.has(content)) continue;
    const panel = content.parentElement;
    if (!panel || panel.hidden) {
      contentBoxes.delete(content);
      continue;
    }
    const size = borderBoxSize?.[0];
    const next = {
      width: size?.inlineSize ?? contentRect.width,
      height: size?.blockSize ?? contentRect.height,
      view,
      nested: content.querySelector("[data-gliding]") !== null,
    };
    if (followBox(content, next) === "glide") benchAround(panel, view);
  }
}

// Moves a watched content's panel for its new box and keeps that box for the next comparison.
function followBox(content, next) {
  const panel = content.parentElement;
  const last = contentBoxes.get(content) ?? null;
  contentBoxes.set(content, next);
  const glide = glides.get(panel) ?? null;
  const move = contentMove({ last, next, glide, motion: canGlide(panel) });
  if (move === "retarget") retargetPanel(panel, glide, next.height);
  else if (move === "glide") glideHeight(panel, last.height, next.height, { target: true });
  return move;
}

function depthOf(node) {
  let depth = 0;
  for (let at = node.parentElement; at; at = at.parentElement) depth++;
  return depth;
}

// A glide started in the callback holds its panel at the old height, so the watched contents around it
// sit out the frame instead of raising a "ResizeObserver loop" error, and rejoin on the next.
function benchAround(panel, view) {
  for (let at = panel.parentElement; at; at = at.parentElement) {
    if (!watched.has(at) || benched.has(at)) continue;
    if (!benched.size) requestAnimationFrame(rejoinBench);
    benched.add(at);
    contentObserver.unobserve(at);
    followBenched(at, view);
  }
}

// Measured with the glides inside it held at their old heights, a benched content shows only a change
// of its own, which glides now rather than being lost with its skipped entry.
function followBenched(content, view) {
  if (!content.parentElement || content.parentElement.hidden) return;
  const { width, height } = content.getBoundingClientRect();
  followBox(content, { width, height, view, nested: false });
}

function rejoinBench() {
  for (const content of benched) if (watched.has(content)) contentObserver.observe(content);
  benched.clear();
}

// A glide that ended inside watched contents may not resize them again, which leaves their stored
// boxes marked mid glide; read them afresh, from `start` out, so their next real change glides.
function settleFrom(start) {
  for (let at = start; at; at = at.parentElement) {
    const box = contentBoxes.get(at);
    if (!box || at.querySelector("[data-gliding]")) continue;
    const { width, height } = at.getBoundingClientRect();
    contentBoxes.set(at, settledBox(box, { width, height }));
  }
}

function retargetPanel(panel, entry, to) {
  const from = panel.getBoundingClientRect().height;
  const margin = [bottomMargin(panel), gapMargin(entry.gap, entry.target)];
  glides.delete(panel);
  entry.animation.cancel();
  glideHeight(panel, from, to, { ...entry, margin });
}

function watchContent(panel) {
  const content = panel?.firstElementChild;
  if (!contentObserver || !content || watched.has(content)) return;
  watched.add(content);
  contentObserver.observe(content);
}

// A rebuilt panel's content inherits what its predecessor last showed, so the change between them
// glides too; the predecessor is no longer watched.
function handOverContent(from, to) {
  const old = from?.firstElementChild;
  const content = to.firstElementChild;
  if (!old || !content) return;
  if (contentBoxes.has(old)) contentBoxes.set(content, contentBoxes.get(old));
  unwatch(old);
}

// The observer holds what it watches, so content that left the page is let go.
function unwatch(content) {
  if (content && watched.delete(content)) contentObserver.unobserve(content);
}

// A box whose own layout just changed eases from the height it was drawn at (`from`) to its new one.
export function glideResize(node, from) {
  stopGlide(node);
  if (!canGlide(node)) return;
  const to = node.getBoundingClientRect().height;
  if (moved(to, from)) glideHeight(node, from, to);
}

// A node that moved within the layout slides over from the client rect it was drawn in (`from`).
export function glideMove(node, from) {
  stopGlide(node);
  if (!canGlide(node)) return;
  const to = node.getBoundingClientRect();
  const x = from.left - to.left;
  const y = from.top - to.top;
  if (!moved(x, 0) && !moved(y, 0)) return;
  const animation = node.animate(
    { transform: [`translate(${x}px, ${y}px)`, "none"] },
    { duration: motionMs("--motion-slow"), easing: motionEase("--ease-glide") },
  );
  track(node, { animation });
}

// Height is the one layout property animated, so what sits below glides instead of jumping; the bottom
// margin rides along only to cancel a column gap (`margin`, from and to). No overshoot: a height that
// overshoots makes everything below it jitter.
function glideHeight(
  node,
  from,
  to,
  { target = null, fade = null, settle = null, gap = 0, margin = null } = {},
) {
  node.dataset.gliding = "true";
  const keyframes = { height: [`${Math.max(0, from)}px`, `${to}px`] };
  if (margin && moved(...margin)) keyframes.marginBottom = margin.map((value) => `${value}px`);
  const animation = node.animate(keyframes, {
    duration: motionMs("--motion-slow"),
    easing: motionEase("--ease-glide"),
    fill: "both",
  });
  track(node, { animation, fade, target, to, settle, gap });
}

// Moves a running glide onto the node that replaced its panel, at the same point in its timing.
function transplant(from, to) {
  const entry = glides.get(from);
  const content = to.firstElementChild;
  const animation = copyAnimation(entry.animation, to);
  const fade = entry.fade && content ? copyAnimation(entry.fade, content) : null;
  stopGlide(from);
  to.dataset.gliding = "true";
  const settle = () => land(to, entry.target);
  track(to, { ...entry, animation, fade, settle });
}

function copyAnimation(animation, node) {
  const copy = node.animate(animation.effect.getKeyframes(), animation.effect.getTiming());
  const placement = copyPlacement(animation);
  if ("startTime" in placement) copy.startTime = placement.startTime;
  else copy.currentTime = placement.currentTime;
  return copy;
}

// Turns a chevron from the transform it showed (`from`, null when its state did not change, so a
// running turn keeps going) to the one [data-open] now gives it.
function turnChevron(chevron, from, { instant }) {
  if (!chevron || from === null) return;
  const running = turnOf(chevron);
  running?.cancel();
  turns.delete(running);
  if (instant || !canGlide(chevron)) return;
  const to = getComputedStyle(chevron).transform;
  if (to === from) return;
  const turn = chevron.animate(
    { transform: [from, to] },
    { duration: motionMs("--motion-spring"), easing: motionEase("--ease-spring") },
  );
  keepTurn(turn);
}

function keepTurn(turn) {
  turns.add(turn);
  turn.onfinish = () => turns.delete(turn);
}

function turnOf(chevron) {
  for (const turn of turns) if (turn.effect?.target === chevron) return turn;
  return null;
}

function track(node, entry) {
  glides.set(node, entry);
  entry.animation.onfinish = () => {
    if (glides.get(node) === entry) endGlide(node, entry.settle);
  };
}

// Lands a glide for good. The panel settles first, so the boxes read around it are final; a ghost
// that settles leaves the page, so where it was is read.
function endGlide(node, settle) {
  const ended = glides.has(node);
  const parent = node.parentElement;
  stopGlide(node);
  settle?.();
  if (ended) settleFrom(parent);
}

function stopGlide(node) {
  const entry = glides.get(node);
  if (!entry) return;
  glides.delete(node);
  entry.animation.cancel();
  entry.fade?.cancel();
  delete node.dataset.gliding;
}

function canGlide(node) {
  return motionEnabled() && typeof node.animate === "function";
}

// The gap `panel` adds to its container, which its margin cancels while it is closed.
function gapOf(panel) {
  const host = panel.parentElement;
  if (!host) return 0;
  const style = getComputedStyle(host);
  return gapShare({
    column: style.display.endsWith("flex") && style.flexDirection === "column",
    rowGap: Number.parseFloat(style.rowGap) || 0,
    alone: ![...host.children].some((child) => child !== panel && inFlow(child)),
  });
}

function inFlow(node) {
  const { display, position } = getComputedStyle(node);
  return display !== "none" && position !== "absolute" && position !== "fixed";
}

function bottomMargin(node) {
  return Number.parseFloat(getComputedStyle(node).marginBottom) || 0;
}

function contentHeight(panel) {
  const content = panel.firstElementChild;
  return content ? content.getBoundingClientRect().height : panel.scrollHeight;
}

function contentOpacity(panel) {
  const content = panel.firstElementChild;
  return content ? Number(getComputedStyle(content).opacity) : null;
}

// Opening, the content rises in just after the panel starts to grow; closing, it fades out first.
// A reversal starts from the opacity the content had, so it never blinks.
function fadeContent(content, open, from) {
  if (!content) return null;
  const easing = motionEase();
  if (!open)
    return content.animate([{ opacity: from ?? 1 }, { opacity: 0 }], {
      duration: motionMs("--motion-fast"),
      easing,
      fill: "forwards",
    });
  const rise = `translateY(calc(${motionToken("--motion-rise") || "0px"} * -1))`;
  return content.animate(
    [
      { opacity: from ?? 0, transform: from === null ? rise : "none" },
      { opacity: 1, transform: "none" },
    ],
    {
      duration: motionMs("--motion-base"),
      delay: from === null ? motionMs("--motion-stagger") : 0,
      easing,
      fill: "backwards",
    },
  );
}

class AccordionManager {
  constructor() {
    this.handleClick = this.handleClick.bind(this);
    document.addEventListener("click", this.handleClick);
  }

  mount(root = document) {
    for (const accordion of root.querySelectorAll("[data-accordion]"))
      this.setOpen(accordion, accordion.dataset.open === "true", { instant: true });
  }

  isOpen(accordion) {
    return accordion?.dataset.open === "true";
  }

  setOpen(accordion, open, { instant = false } = {}) {
    const parts = accordionParts(accordion);
    if (!parts) return;
    const { trigger, content } = parts;
    const target = open === true;
    const chevron = chevronOf(trigger);
    const changed = accordion.dataset.open !== String(target);
    const shown = chevron && changed ? getComputedStyle(chevron).transform : null;
    delayFromOpen(accordion, target);
    if (!target) handOffFocus(content, trigger);
    markOpen(accordion, parts, target);
    turnChevron(chevron, shown, { instant });
    setPanelOpen(content, target, { instant });
    disclosures.set(accordion.dataset.disclosure, { open: target, panel: content, chevron });
  }

  handleClick(event) {
    const trigger = event.target.closest("[data-accordion-trigger]");
    if (!trigger) return;
    const accordion = trigger.closest("[data-accordion]");
    if (!accordion) return;
    const open = !this.isOpen(accordion);
    this.setOpen(accordion, open);
    // Only a press announces itself, and only after the panel has been toggled: a listener may
    // re-render the card from here, and the fresh node carries on from its disclosure key.
    accordion.dispatchEvent(new CustomEvent(ACCORDION_TOGGLE, { detail: { open } }));
  }
}

export function createAccordion(key, className, label, ...children) {
  const id = `accordion-${safeId(key)}-${++sequence}`;
  const accordion = document.createElement("section");
  accordion.className = `accordion ${className}`;
  accordion.dataset.accordion = "";
  accordion.dataset.disclosure = key;

  const trigger = document.createElement("button");
  trigger.className = "accordion-trigger";
  trigger.type = "button";
  trigger.id = `${id}-trigger`;
  trigger.dataset.accordionTrigger = "";
  trigger.dataset.focusKey = `disclosure-${key}`;
  trigger.setAttribute("aria-controls", `${id}-content`);
  const title = document.createElement("span");
  title.className = "accordion-label";
  title.textContent = label;
  trigger.append(title, chevronIcon());

  const content = document.createElement("div");
  content.className = "accordion-panel";
  content.id = `${id}-content`;
  content.setAttribute("role", "region");
  content.setAttribute("aria-labelledby", trigger.id);
  const inner = document.createElement("div");
  inner.className = "accordion-panel-inner";
  inner.append(...children.filter(Boolean));
  content.append(inner);
  accordion.append(trigger, content);
  resume(accordion, { trigger, content });
  return accordion;
}

// A fresh node picks up where its key's last node was: in the state it asked for, still gliding
// toward it if it was.
function resume(accordion, parts) {
  const key = accordion.dataset.disclosure;
  const last = disclosures.get(key);
  const gliding = Boolean(last && glides.has(last.panel));
  const start = rebuiltDisclosure({ open: last?.open, gliding });
  markOpen(accordion, parts, start.open);
  parts.content.hidden = start.hidden;
  const chevron = chevronOf(parts.trigger);
  if (start.glide) transplant(last.panel, parts.content);
  if (last) handOverContent(last.panel, parts.content);
  const turn = last?.chevron ? turnOf(last.chevron) : null;
  if (chevron && turn?.playState === "running" && canGlide(chevron))
    keepTurn(copyAnimation(turn, chevron));
  delayFromOpen(accordion, start.open);
  if (!start.hidden) watchContent(parts.content);
  disclosures.set(key, { open: start.open, panel: parts.content, chevron });
}

// CSS animations inside a disclosure count from when it opened, so a rebuilt node carries them on.
function delayFromOpen(accordion, open) {
  const elapsed = changedAgo(`disclosure-${accordion.dataset.disclosure}`, open);
  if (Number.isFinite(elapsed))
    accordion.style.setProperty("--open-delay", `${-Math.round(elapsed)}ms`);
  else accordion.style.removeProperty("--open-delay");
}

function markOpen(accordion, { trigger, content }, open) {
  accordion.dataset.open = String(open);
  trigger.setAttribute("aria-expanded", String(open));
  content.inert = !open;
}

function chevronOf(trigger) {
  return trigger.querySelector(":scope > .accordion-chevron");
}

function accordionParts(accordion) {
  const trigger = accordion.querySelector(":scope > [data-accordion-trigger]");
  const content = accordion.querySelector(":scope > .accordion-panel");
  return trigger && content ? { trigger, content } : null;
}

function chevronIcon() {
  const node = icon("chevron-right", 12);
  node.classList.add("accordion-chevron");
  return node;
}

function safeId(value) {
  return (
    String(value)
      .replace(/[^a-zA-Z0-9_-]+/g, "-")
      .slice(0, 80) || "content"
  );
}

export const accordionManager = new AccordionManager();
