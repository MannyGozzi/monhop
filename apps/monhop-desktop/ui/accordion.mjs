import { motionEase, motionMs, motionToken } from "./dom.mjs";
import { panelNeedsChange } from "./glide-model.mjs";
import { icon } from "./icons.mjs";

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
let sequence = 0;
// The running glide of each element it moves; a new glide on that element replaces it from where it is.
const glides = new Map();
reducedMotion.addEventListener("change", () => {
  for (const [node, entry] of glides) {
    stopGlide(node);
    entry.settle?.();
  }
});

// Fired on the accordion element when the user toggles it, never when a render reopens it.
export const ACCORDION_TOGGLE = "accordion-toggle";

// Opens or closes a clipping panel by gliding its height between 0 and its content's (its one
// child), which fades in step. Asking again for the state it has or is heading to does nothing, so a
// re-render never restarts or cuts a glide; the other state reverses from what is on screen.
export function setPanelOpen(panel, open, { instant = false } = {}) {
  const target = open === true;
  const running = glides.get(panel);
  const glidingTo = running ? running.target : null;
  if (!panelNeedsChange({ glidingTo, hidden: panel.hidden, open: target, instant })) return;
  const settle = () => {
    panel.hidden = !target;
  };
  if (instant || !canGlide(panel)) {
    stopGlide(panel);
    settle();
    return;
  }
  const from = running || !target ? panel.getBoundingClientRect().height : 0;
  const opacity = running ? contentOpacity(panel) : null;
  stopGlide(panel);
  panel.hidden = false;
  const to = target ? contentHeight(panel) : 0;
  if (Math.abs(to - from) < 0.5) {
    settle();
    return;
  }
  const fade = fadeContent(panel.firstElementChild, target, opacity);
  glideHeight(panel, from, to, { target, fade, settle });
}

// Content that changed height under an opening glide moves where the glide lands, instead of the
// panel snapping to it once the glide ends.
export function retargetPanel(panel) {
  const entry = glides.get(panel);
  if (entry?.target !== true) return;
  const to = contentHeight(panel);
  if (Math.abs(to - entry.to) < 0.5) return;
  const from = panel.getBoundingClientRect().height;
  glides.delete(panel);
  entry.animation.cancel();
  glideHeight(panel, from, to, entry);
}

// A box whose own layout just changed eases from the height it was drawn at (`from`) to its new one.
export function glideResize(node, from) {
  stopGlide(node);
  if (!canGlide(node)) return;
  const to = node.getBoundingClientRect().height;
  if (Math.abs(to - from) >= 0.5) glideHeight(node, from, to);
}

// A node that moved within the layout slides over from the client rect it was drawn in (`from`).
export function glideMove(node, from) {
  stopGlide(node);
  if (!canGlide(node)) return;
  const to = node.getBoundingClientRect();
  const x = from.left - to.left;
  const y = from.top - to.top;
  if (Math.abs(x) < 0.5 && Math.abs(y) < 0.5) return;
  const animation = node.animate(
    { transform: [`translate(${x}px, ${y}px)`, "none"] },
    { duration: motionMs("--motion-slow"), easing: motionEase("--ease-glide") },
  );
  track(node, { animation });
}

// Height is the one layout property animated, so what sits below glides instead of jumping. No
// overshoot: a height that overshoots makes everything below it jitter.
function glideHeight(node, from, to, { target = null, fade = null, settle = null } = {}) {
  node.dataset.gliding = "true";
  const animation = node.animate(
    { height: [`${Math.max(0, from)}px`, `${to}px`] },
    { duration: motionMs("--motion-slow"), easing: motionEase("--ease-glide"), fill: "both" },
  );
  track(node, { animation, fade, target, to, settle });
}

function track(node, entry) {
  glides.set(node, entry);
  entry.animation.onfinish = () => {
    if (glides.get(node) !== entry) return;
    stopGlide(node);
    entry.settle?.();
  };
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
  return !reducedMotion.matches && typeof node.animate === "function";
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
    accordion.dataset.open = String(target);
    trigger.setAttribute("aria-expanded", String(target));
    if (!target && content.contains(document.activeElement)) trigger.focus({ preventScroll: true });
    content.inert = !target;
    setPanelOpen(content, target, { instant });
  }

  handleClick(event) {
    const trigger = event.target.closest("[data-accordion-trigger]");
    if (!trigger) return;
    const accordion = trigger.closest("[data-accordion]");
    if (!accordion) return;
    const open = !this.isOpen(accordion);
    this.setOpen(accordion, open);
    // Only a press announces itself, and only after the panel has been toggled: a listener may
    // re-render the card from here, and the fresh node is reopened from its disclosure key.
    accordion.dispatchEvent(new CustomEvent(ACCORDION_TOGGLE, { detail: { open } }));
  }
}

export function createAccordion(key, className, label, ...children) {
  const id = `accordion-${safeId(key)}-${++sequence}`;
  const accordion = document.createElement("section");
  accordion.className = `accordion ${className}`;
  accordion.dataset.accordion = "";
  accordion.dataset.disclosure = key;
  accordion.dataset.open = "false";

  const trigger = document.createElement("button");
  trigger.className = "accordion-trigger";
  trigger.type = "button";
  trigger.id = `${id}-trigger`;
  trigger.dataset.accordionTrigger = "";
  trigger.dataset.focusKey = `disclosure-${key}`;
  trigger.setAttribute("aria-expanded", "false");
  trigger.setAttribute("aria-controls", `${id}-content`);
  const title = document.createElement("span");
  title.className = "accordion-label";
  title.textContent = label;
  trigger.append(title, chevron());

  const content = document.createElement("div");
  content.className = "accordion-panel";
  content.id = `${id}-content`;
  content.setAttribute("role", "region");
  content.setAttribute("aria-labelledby", trigger.id);
  content.hidden = true;
  content.inert = true;
  const inner = document.createElement("div");
  inner.className = "accordion-panel-inner";
  inner.append(...children.filter(Boolean));
  content.append(inner);
  accordion.append(trigger, content);
  return accordion;
}

function accordionParts(accordion) {
  const trigger = accordion.querySelector(":scope > [data-accordion-trigger]");
  const content = accordion.querySelector(":scope > .accordion-panel");
  return trigger && content ? { trigger, content } : null;
}

function chevron() {
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
