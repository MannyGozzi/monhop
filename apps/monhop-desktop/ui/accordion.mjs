import { icon } from "./icons.mjs";
let sequence = 0;

// Fired on the accordion element when the user toggles it, never when a render reopens it.
export const ACCORDION_TOGGLE = "accordion-toggle";

class AccordionManager {
  constructor() {
    this.handleClick = this.handleClick.bind(this);
    document.addEventListener("click", this.handleClick);
  }

  mount(root = document) {
    for (const accordion of root.querySelectorAll("[data-accordion]")) {
      const open = accordion.dataset.open === "true";
      this.setOpen(accordion, open);
    }
  }

  isOpen(accordion) {
    return accordion?.dataset.open === "true";
  }

  setOpen(accordion, open) {
    const parts = accordionParts(accordion);
    if (!parts) return;
    const { trigger, content } = parts;
    const target = open === true;
    if (this.isOpen(accordion) === target && content.hidden === !target) return;

    accordion.dataset.open = String(target);
    trigger.setAttribute("aria-expanded", String(target));
    if (target) {
      content.hidden = false;
      content.inert = false;
    } else {
      if (content.contains(document.activeElement)) trigger.focus({ preventScroll: true });
      content.inert = true;
    }

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
  inner.append(...children);
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
