import { glideMove } from "./accordion.mjs";
import {
  handOffFocus,
  motionEase,
  motionEnabled,
  motionMs,
  motionToken,
  reducedMotion,
  settle,
} from "./dom.mjs";
import { slotChange, slotPop } from "./header-motion-model.mjs";

// Each slot's target and the pop it is running toward it.
const slots = new Map();
reducedMotion.addEventListener("change", () => {
  for (const entry of slots.values()) entry.pop?.finish();
});

// Puts a persistent slot in or out of its row. The row reflows at once: `movers` glide over from
// where they were drawn and the slot pops in place. Leaving, it hands its focus to `focusTarget`.
export function setSlotPresent(slot, present, movers = [], { focusTarget = null } = {}) {
  const entry = slots.get(slot);
  const change = slotChange({ shown: entry?.present ?? null, present, motion: motionEnabled() });
  if (!change) return;
  const drawn = change.animate ? movers.map((node) => node.getBoundingClientRect()) : [];
  const from = change.animate && entry?.pop ? look(slot) : null;
  entry?.pop?.cancel();
  const next = { present, pop: null };
  slots.set(slot, next);
  if (!present) handOffFocus(slot, focusTarget);
  slot.inert = !present;
  slot.hidden = !present && !change.animate;
  // A leaving slot is out of the row already but stays drawn in its old spot until its pop ends.
  slot.dataset.leaving = String(!present && change.animate);
  if (!change.animate) return;
  for (const [index, node] of movers.entries()) glideMove(node, drawn[index]);
  const goneScale = Number.parseFloat(motionToken("--scale-pop"));
  next.pop = pop(slot, present, slotPop({ present, from, goneScale }));
  void settle(next.pop, () => {
    if (slots.get(slot) !== next) return;
    next.pop = null;
    if (present) return;
    slot.hidden = true;
    slot.dataset.leaving = "false";
  });
}

// In with the house pop-in spring; out fast, gone before the gliding neighbor covers the spot.
function pop(node, present, { from, to, wait }) {
  return node.animate(
    {
      opacity: [from.opacity, to.opacity],
      transform: [`scale(${from.scale})`, `scale(${to.scale})`],
    },
    {
      duration: motionMs(present ? "--motion-spring" : "--motion-fast"),
      easing: motionEase(present ? "--ease-spring" : "--ease-out"),
      delay: wait ? motionMs("--motion-stagger") : 0,
      fill: "both",
    },
  );
}

function look(node) {
  const style = getComputedStyle(node);
  const scale = style.transform === "none" ? 1 : new DOMMatrixReadOnly(style.transform).a;
  return { opacity: Number(style.opacity), scale };
}
