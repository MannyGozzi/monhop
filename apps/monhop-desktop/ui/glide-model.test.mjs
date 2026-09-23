import assert from "node:assert/strict";
import test from "node:test";

import {
  SUBPIXEL_PX,
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

test("a settled panel changes only when asked for the other state", () => {
  assert.equal(panelNeedsChange({ hidden: true, open: false }), false);
  assert.equal(panelNeedsChange({ hidden: false, open: true }), false);
  assert.equal(panelNeedsChange({ hidden: true, open: true }), true);
  assert.equal(panelNeedsChange({ hidden: false, open: false }), true);
});

test("a re-render that repeats a gliding panel's target leaves the glide running", () => {
  // Mid-close the panel is not hidden yet, so `hidden` alone would restart the close; the target decides.
  assert.equal(panelNeedsChange({ glidingTo: true, hidden: false, open: true }), false);
  assert.equal(panelNeedsChange({ glidingTo: false, hidden: false, open: false }), false);
});

test("the other target reverses a glide, and an instant request lands it", () => {
  assert.equal(panelNeedsChange({ glidingTo: true, hidden: false, open: false }), true);
  assert.equal(panelNeedsChange({ glidingTo: false, hidden: false, open: true }), true);
  assert.equal(
    panelNeedsChange({ glidingTo: true, hidden: false, open: true, instant: true }),
    true,
  );
});

test("a rebuilt disclosure carries on an opening glide exactly as it carries on a closing one", () => {
  const opening = rebuiltDisclosure({ open: true, gliding: true });
  const closing = rebuiltDisclosure({ open: false, gliding: true });
  assert.deepEqual(opening, { open: true, hidden: false, glide: true });
  assert.deepEqual(closing, { open: false, hidden: false, glide: true });
  assert.equal(opening.glide, closing.glide);
});

test("a rebuilt disclosure at rest starts in the state its key last asked for", () => {
  assert.deepEqual(rebuiltDisclosure({ open: true }), { open: true, hidden: false, glide: false });
  assert.deepEqual(rebuiltDisclosure({ open: false }), { open: false, hidden: true, glide: false });
  assert.deepEqual(rebuiltDisclosure(), { open: false, hidden: true, glide: false });
});

const box = (height, extra = {}) => ({
  width: 600,
  height,
  view: "980x720",
  nested: false,
  ...extra,
});

test("an open panel at rest glides when its content changes height, and only then", () => {
  assert.equal(contentMove({ last: box(40), next: box(120) }), "glide");
  assert.equal(contentMove({ last: box(120), next: box(40) }), "glide");
  assert.equal(contentMove({ last: box(40), next: box(40.2) }), "none");
  assert.equal(contentMove({ last: null, next: box(40) }), "none");
});

test("an opening glide heads for new content; a closing one ignores it", () => {
  const opening = { target: true, to: 40 };
  assert.equal(contentMove({ last: box(40), next: box(120), glide: opening }), "retarget");
  assert.equal(contentMove({ last: box(40), next: box(40), glide: opening }), "none");
  // A rebuilt node's first sighting still retargets the glide it carried over.
  assert.equal(contentMove({ last: null, next: box(120), glide: opening }), "retarget");
  assert.equal(
    contentMove({ last: box(40), next: box(120), glide: { target: false, to: 0 } }),
    "none",
  );
});

test("reflows and changes a nested glide is animating follow at once", () => {
  assert.equal(contentMove({ last: box(40), next: box(80, { width: 420 }) }), "none");
  assert.equal(contentMove({ last: box(40), next: box(80, { view: "760x560" }) }), "none");
  assert.equal(contentMove({ last: box(40), next: box(80, { nested: true }) }), "none");
  // The frame a nested glide ends on still belongs to it.
  assert.equal(contentMove({ last: box(78, { nested: true }), next: box(80) }), "none");
});

test("without motion nothing glides", () => {
  assert.equal(contentMove({ last: box(40), next: box(120), motion: false }), "none");
  assert.equal(
    contentMove({ last: box(40), next: box(120), glide: { target: true, to: 40 }, motion: false }),
    "none",
  );
});

test("height glides live only in accordion.mjs, so every collapsible moves one way", async () => {
  const { readdir, readFile } = await import("node:fs/promises");
  const files = (await readdir(new URL(".", import.meta.url))).filter(
    (name) => /\.(m?js)$/.test(name) && !name.endsWith(".test.mjs") && name !== "accordion.mjs",
  );
  const sources = await Promise.all(
    files.map((file) => readFile(new URL(file, import.meta.url), "utf8")),
  );
  for (const [index, source] of sources.entries())
    assert.doesNotMatch(source, /height:\s*\[/, files[index]);
});

test("a box read afresh when a nested glide ends lets the next real change glide", () => {
  // The observer last saw the section mid nested glide and never fired as it landed.
  const stale = box(78, { nested: true });
  assert.equal(contentMove({ last: stale, next: box(140) }), "none");
  const fresh = settledBox(stale, { width: 600, height: 80 });
  assert.deepEqual(fresh, box(80));
  assert.equal(contentMove({ last: fresh, next: box(140) }), "glide");
});

test("a copied animation keeps the original's start time, not a held current time", () => {
  assert.deepEqual(copyPlacement({ startTime: 1000, currentTime: 120 }), { startTime: 1000 });
  // An original still waiting to start has only its current time to hand over.
  assert.deepEqual(copyPlacement({ startTime: null, currentTime: 0 }), { currentTime: 0 });
});

test("one sub-pixel threshold decides every glide", () => {
  assert.equal(moved(40, 40 + SUBPIXEL_PX), true);
  assert.equal(moved(40, 40 + SUBPIXEL_PX * 0.9), false);
  assert.equal(contentMove({ last: box(40), next: box(40 + SUBPIXEL_PX * 0.9) }), "none");
  assert.equal(contentMove({ last: box(40), next: box(40 + SUBPIXEL_PX) }), "glide");
});

test("the threshold has one home", async () => {
  const { readFile } = await import("node:fs/promises");
  const files = ["accordion.mjs", "glide-model.mjs", "header-motion.mjs"];
  const sources = await Promise.all(
    files.map((file) => readFile(new URL(file, import.meta.url), "utf8")),
  );
  for (const [index, source] of sources.entries()) {
    const literals = source.match(/(?<![\w.])0?\.5(?![\d%])/g) ?? [];
    assert.equal(literals.length, files[index] === "glide-model.mjs" ? 1 : 0, files[index]);
  }
});

test("a content change beside a starting nested glide glides, then follows that glide", () => {
  // Measured with the nested glide held at its old height, only the content's own new row shows.
  assert.equal(contentMove({ last: box(300), next: box(350) }), "glide");
  // Once back under observation the nested growth arrives while the own glide still runs.
  const own = { target: true, to: 350 };
  assert.equal(
    contentMove({ last: box(350), next: box(380, { nested: true }), glide: own }),
    "retarget",
  );
});

const drew = (extra = {}) => ({ drawn: true, hidden: false, gliding: false, ...extra });

test("a block first seen, or seen without motion, is drawn at rest or not at all", () => {
  const rest = { ghost: false, hidden: false, glide: false };
  assert.deepEqual(rebuiltPresence({ present: true }), rest);
  assert.equal(rebuiltPresence({ present: false }), null);
  // Without motion nothing glides and nothing that left lingers, whatever the last render drew.
  assert.deepEqual(
    rebuiltPresence({ present: true, motion: false, last: drew({ gliding: true }) }),
    rest,
  );
  assert.equal(
    rebuiltPresence({ present: false, motion: false, last: drew({ gliding: true }) }),
    null,
  );
});

test("a block that appears starts closed, and one that leaves draws a ghost from its full height", () => {
  assert.deepEqual(rebuiltPresence({ present: true, last: { drawn: false } }), {
    ghost: false,
    hidden: true,
    glide: false,
  });
  assert.deepEqual(rebuiltPresence({ present: false, last: drew() }), {
    ghost: true,
    hidden: false,
    glide: false,
  });
});

test("a block rebuilt mid-glide carries the glide on either way, so a flip reverses it", () => {
  const gliding = drew({ gliding: true });
  assert.deepEqual(rebuiltPresence({ present: true, last: gliding }), {
    ghost: false,
    hidden: false,
    glide: true,
  });
  assert.deepEqual(rebuiltPresence({ present: false, last: gliding }), {
    ghost: true,
    hidden: false,
    glide: true,
  });
});

test("a rebuild before a glide starts keeps the block where it was drawn", () => {
  // Built closed but not yet opened: still closed. Built as a ghost but not yet closing: still whole.
  assert.equal(rebuiltPresence({ present: true, last: drew({ hidden: true }) }).hidden, true);
  assert.equal(rebuiltPresence({ present: false, last: drew() }).hidden, false);
  // A ghost whose glide ended is gone, and one that never showed is never drawn.
  assert.equal(rebuiltPresence({ present: false, last: drew({ hidden: true }) }), null);
  assert.equal(rebuiltPresence({ present: false, last: { drawn: false } }), null);
  // Coming back after it left, it grows from closed again, as when it first appeared.
  assert.deepEqual(
    rebuiltPresence({ present: true, last: drew({ hidden: true }) }),
    rebuiltPresence({ present: true, last: { drawn: false } }),
  );
});

test("only a flex column's gap is claimed, and only beside another block", () => {
  assert.equal(gapShare({ column: true, rowGap: 16, alone: false }), 16);
  assert.equal(gapShare({ column: true, rowGap: 16, alone: true }), 0);
  assert.equal(gapShare({ column: false, rowGap: 16, alone: false }), 0);
  assert.equal(gapShare({ column: true, rowGap: 0, alone: false }), 0);
});

test("a closed block adds nothing to its column and an open one its height plus one gap", () => {
  const gap = 16;
  const height = 120;
  // A column of n blocks has n - 1 gaps, so the block adds one gap, its height and its margin.
  const added = (progress) => {
    const margin =
      gapMargin(gap, false) + (gapMargin(gap, true) - gapMargin(gap, false)) * progress;
    return gap + height * progress + margin;
  };
  assert.equal(added(0), 0);
  assert.equal(added(1), gap + height);
  // Height and margin share one easing, so the column moves on one curve and never steps.
  for (const progress of [0.25, 0.5, 0.75])
    assert.equal(added(progress), (gap + height) * progress);
  assert.equal(gapMargin(0, false), 0);
  assert.equal(gapMargin(undefined, false), 0);
});
