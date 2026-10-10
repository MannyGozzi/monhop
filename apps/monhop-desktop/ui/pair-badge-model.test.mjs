import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";

import { ICONS } from "./icons.mjs";
import {
  PAIR_BADGE_COLORS,
  PAIR_BADGE_SYMBOLS,
  normalizePairBadge,
  pairBadgeLabel,
  pairBadgeParts,
} from "./pair-badge-model.mjs";

test("the badge vocabulary has the sizes the backend indexes into, with no repeats", () => {
  assert.equal(PAIR_BADGE_COLORS.length, 8);
  assert.equal(PAIR_BADGE_SYMBOLS.length, 32);
  for (const list of [
    PAIR_BADGE_COLORS.map((color) => color.key),
    PAIR_BADGE_COLORS.map((color) => color.name),
    PAIR_BADGE_SYMBOLS.map((symbol) => symbol.icon),
    PAIR_BADGE_SYMBOLS.map((symbol) => symbol.name),
  ])
    assert.equal(new Set(list).size, list.length);
  for (const symbol of PAIR_BADGE_SYMBOLS) assert.ok(symbol.icon in ICONS, symbol.icon);
});

test("the order is part of the pairing contract between versions", () => {
  assert.deepEqual(
    PAIR_BADGE_COLORS.map((color) => color.key),
    ["violet", "blue", "sky", "teal", "lime", "brown", "pink", "slate"],
  );
  assert.equal(PAIR_BADGE_SYMBOLS[0].icon, "anchor");
  assert.equal(PAIR_BADGE_SYMBOLS[21].icon, "moon");
  assert.equal(PAIR_BADGE_SYMBOLS[31].icon, "zap");
});

test("every badge color has a light and a dark token in styles.css", async () => {
  const css = await readFile(new URL("./styles.css", import.meta.url), "utf8");
  for (const { key } of PAIR_BADGE_COLORS) {
    assert.match(css, new RegExp(`--pair-${key}: light-dark\\(`), key);
    assert.match(
      css,
      new RegExp(`\\[data-color="${key}"\\] \\{ --tone: var\\(--pair-${key}\\); \\}`),
    );
  }
});

test("only in-range badges are drawn", () => {
  assert.deepEqual(normalizePairBadge({ color: 7, symbols: [31, 0, 5] }), {
    color: 7,
    symbols: [31, 0, 5],
  });
  for (const value of [
    null,
    [],
    { color: 8, symbols: [0, 1, 2] },
    { color: -1, symbols: [0, 1, 2] },
    { color: 1.5, symbols: [0, 1, 2] },
    { color: "1", symbols: [0, 1, 2] },
    { color: 1, symbols: [0, 1] },
    { color: 1, symbols: [0, 1, 2, 3] },
    { color: 1, symbols: [0, 1, 32] },
    { color: 1, symbols: "012" },
  ])
    assert.equal(normalizePairBadge(value), null, JSON.stringify(value));
});

test("the label names the color and each symbol, as people read it aloud", () => {
  const badge = { color: 0, symbols: [0, 21, 19] };
  assert.equal(pairBadgeLabel(badge), "Violet: anchor, moon, leaf");
  assert.deepEqual(pairBadgeParts(badge).color, { key: "violet", name: "Violet" });
  assert.deepEqual(
    pairBadgeParts({ color: 7, symbols: [10, 20, 31] }).symbols.map((symbol) => symbol.name),
    ["cup", "light bulb", "lightning"],
  );
});
