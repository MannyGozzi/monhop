// A move or resize smaller than this is not drawn differently, so nothing animates below it.
export const SUBPIXEL_PX = 0.5;

// Whether asking a panel for `open` changes anything. A panel settled in that state, or already
// gliding toward it, is left alone, so a re-render never restarts or cuts its glide.
export function panelNeedsChange({ glidingTo = null, hidden, open, instant = false }) {
  if (glidingTo !== null) return glidingTo !== open || instant;
  return hidden === open;
}

// How a disclosure that a render rebuilt starts: in the state its key last asked for (`open`), and
// still gliding if the node it replaces was (`gliding`), whichever way. Landing it instead snaps it.
export function rebuiltDisclosure({ open = false, gliding = false } = {}) {
  return { open, hidden: !open && !gliding, glide: gliding };
}

// How a keyed block that comes and goes starts when a render rebuilds it, from what its key's last
// render drew (`last`, null at first sight): `ghost` draws a copy of what left, `hidden` a block that has
// not grown yet, and `glide` carries on the running glide, whichever way. Null draws nothing.
export function rebuiltPresence({ present, motion = true, last = null }) {
  if (!motion || !last) return present ? { ghost: false, hidden: false, glide: false } : null;
  if (last.gliding) return { ghost: !present, hidden: false, glide: true };
  if (!present && (!last.drawn || last.hidden)) return null;
  return { ghost: !present, hidden: !last.drawn || last.hidden, glide: false };
}

// The gap a block adds to its container: a flex column's row gap once the block has an in-flow
// sibling. A grid row never shrinks below 0, so no margin can cancel a grid gap and none is claimed.
export function gapShare({ column, rowGap, alone }) {
  return column && !alone && rowGap > 0 ? rowGap : 0;
}

// The bottom margin that cancels that gap while the block is closed, so gliding with its height the
// column grows by exactly what the block adds and nothing below it jumps.
export function gapMargin(gap, open) {
  return open || !gap ? 0 : -gap;
}

// What a panel does when its content's box goes from `last` to `next`: an opening glide retargets and
// a panel at rest glides. First sightings, closing, reflows and nested glides follow at once.
export function contentMove({ last = null, next, glide = null, motion = true }) {
  if (!motion) return "none";
  if (glide) return glide.target === true && moved(next.height, glide.to) ? "retarget" : "none";
  if (!last || last.nested || next.nested || last.view !== next.view) return "none";
  if (moved(next.width, last.width)) return "none";
  return moved(next.height, last.height) ? "glide" : "none";
}

export function moved(a, b) {
  return Math.abs(a - b) >= SUBPIXEL_PX;
}

// A content box read afresh once a glide inside it ended; the observer need not fire as it lands, and
// a box left nested would make the next real change snap.
export function settledBox(box, size) {
  return { ...box, ...size, nested: false };
}

// Where a copied animation starts: on the original's start time when it has one, since a copy given
// only the current time holds it for a frame while it waits to start.
export function copyPlacement({ startTime, currentTime }) {
  return startTime === null || startTime === undefined ? { currentTime } : { startTime };
}
