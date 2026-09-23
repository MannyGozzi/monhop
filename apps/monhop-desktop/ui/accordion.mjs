import { motionEase, motionMs, motionToken } from "./dom.mjs";
import { icon } from "./icons.mjs";

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
let sequence = 0;

// Fired on the accordion element when the user toggles it, never when a render reopens it.
export const ACCORDION_TOGGLE = "accordion-toggle";

class AccordionManager {
  constructor() {
    this.animations = new Map();
    this.handleClick = this.handleClick.bind(this);
    this.handleMotionChange = this.handleMotionChange.bind(this);
    document.addEventListener("click", this.handleClick);
    reducedMotion.addEventListener("change", this.handleMotionChange);
  }

  mount(root = document) {
    this.disposeDetached();
    for (const accordion of root.querySelectorAll("[data-accordion]")) {
      const open = accordion.dataset.open === "true";
      this.setOpen(accordion, open, { instant: true });
    }
  }

  isOpen(accordion) {
    return accordion?.dataset.open === "true";
  }

  setOpen(accordion, open, { instant = false } = {}) {
    const parts = accordionParts(accordion);
    if (!parts) return;
    const { trigger, content } = parts;
    const target = open === true;
    const running = this.animations.get(accordion);
    if (!running && this.isOpen(accordion) === target && content.hidden === !target) return;

    // A press mid-animation reverses from the current height instead of jumping to an end.
    const start = running
      ? content.getBoundingClientRect().height
      : target
        ? 0
        : content.getBoundingClientRect().height;
    const opacity = running ? contentOpacity(content) : null;
    if (running) {
      stop(running);
      this.animations.delete(accordion);
    }

    accordion.dataset.open = String(target);
    trigger.setAttribute("aria-expanded", String(target));
    if (target) {
      content.hidden = false;
      content.inert = false;
    } else {
      if (content.contains(document.activeElement)) trigger.focus({ preventScroll: true });
      content.inert = true;
    }

    if (instant || reducedMotion.matches || typeof content.animate !== "function") {
      this.finish(accordion, target);
      return;
    }

    content.style.height = `${Math.max(0, start)}px`;
    const end = target ? content.scrollHeight : 0;
    if (start === end) {
      this.finish(accordion, target);
      return;
    }
    // Height is the one layout property animated: a small panel, so content below glides instead of jumping.
    const animation = content.animate(
      { height: [`${Math.max(0, start)}px`, `${end}px`] },
      { duration: motionMs("--motion-slow"), easing: motionEase(), fill: "both" },
    );
    const entry = { animation, fade: fadeContent(content, target, opacity), target };
    this.animations.set(accordion, entry);
    animation.onfinish = () => {
      if (this.animations.get(accordion) !== entry) return;
      this.animations.delete(accordion);
      this.finish(accordion, target);
      stop(entry);
    };
  }

  disposeDetached() {
    for (const [accordion, entry] of this.animations) {
      if (accordion.isConnected) continue;
      stop(entry);
      this.animations.delete(accordion);
    }
  }

  finish(accordion, open) {
    const parts = accordionParts(accordion);
    if (!parts) return;
    const { content } = parts;
    content.style.height = "";
    content.hidden = !open;
    content.inert = !open;
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

  handleMotionChange() {
    for (const [accordion, entry] of this.animations) {
      stop(entry);
      this.animations.delete(accordion);
      this.finish(accordion, entry.target);
    }
  }
}

function stop(entry) {
  entry.animation.cancel();
  entry.fade?.cancel();
}

function contentOpacity(content) {
  const inner = content.firstElementChild;
  return inner ? Number(getComputedStyle(inner).opacity) : null;
}

// Opening, the content rises in just after the panel starts to grow; closing, it fades out first.
// A reversal starts from the opacity the content had, so it never blinks.
function fadeContent(content, open, from) {
  const inner = content.firstElementChild;
  if (!inner) return null;
  const easing = motionEase();
  if (!open)
    return inner.animate([{ opacity: from ?? 1 }, { opacity: 0 }], {
      duration: motionMs("--motion-fast"),
      easing,
      fill: "forwards",
    });
  const rise = `translateY(calc(${motionToken("--motion-rise") || "0px"} * -1))`;
  return inner.animate(
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
