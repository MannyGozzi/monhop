import assert from "node:assert/strict";
import test from "node:test";

import {
  MENU_GAP,
  MENU_INSET,
  displayOwner,
  displaySections,
  menuPlacement,
  thumbnailRects,
} from "./canvas-menus-model.mjs";

const bounds = { left: 0, top: 0, right: 800, bottom: 600 };
const box = (left, top, width, height) => ({
  left,
  top,
  width,
  height,
  right: left + width,
  bottom: top + height,
});

test("a menu sits on its preferred side when it fits, centred on the anchor and growing from it", () => {
  const placed = menuPlacement({
    anchor: box(300, 100, 200, 120),
    size: { width: 300, height: 200 },
    bounds,
  });
  assert.deepEqual(placed, { x: 250, y: 220 + MENU_GAP, side: "below", origin: 150 });
});

test("a menu flips to the side with room, then to the roomier side, and stays inside the pane", () => {
  const low = menuPlacement({
    anchor: box(300, 450, 200, 100),
    size: { width: 300, height: 200 },
    bounds,
  });
  assert.equal(low.side, "above");
  assert.equal(low.y, 450 - MENU_GAP - 200);
  const tall = menuPlacement({
    anchor: box(300, 380, 200, 100),
    size: { width: 300, height: 500 },
    bounds,
  });
  assert.equal(tall.side, "above");
  assert.equal(tall.y, MENU_INSET);
});

test("a menu along an anchor's start edge clamps at the pane's edge and keeps its origin on the anchor", () => {
  const edge = menuPlacement({
    anchor: box(700, 520, 90, 30),
    size: { width: 300, height: 240 },
    bounds,
    prefer: "above",
    align: "start",
  });
  assert.equal(edge.side, "above");
  assert.equal(edge.x, 800 - MENU_INSET - 300);
  assert.equal(edge.origin, 700 + 45 - edge.x);
  const start = menuPlacement({
    anchor: box(40, 520, 120, 30),
    size: { width: 300, height: 240 },
    bounds,
    prefer: "above",
    align: "start",
  });
  assert.equal(start.x, 40);
  assert.equal(start.origin, 60);
});

test("a thumbnail keeps the arrangement's proportions, centred in its box", () => {
  const rects = thumbnailRects(
    [
      { id: "a", group: "local", x: 0, y: 0, width: 200, height: 100 },
      { id: "b", group: "peer", x: 200, y: 50, width: 200, height: 100 },
    ],
    { width: 40, height: 30 },
  );
  assert.deepEqual(rects, [
    { id: "a", group: "local", x: 0, y: 7.5, width: 20, height: 10 },
    { id: "b", group: "peer", x: 20, y: 12.5, width: 20, height: 10 },
  ]);
  assert.deepEqual(thumbnailRects([], { width: 40, height: 30 }), []);
  const inset = thumbnailRects(
    [{ id: "a", x: 0, y: 0, width: 40, height: 30 }],
    { width: 40, height: 30 },
    1,
  );
  assert.deepEqual(inset, [{ id: "a", group: undefined, x: 1, y: 1, width: 38, height: 28 }]);
});

test("the displays menu lists each computer's displays, one computer when scoped, the opener marked", () => {
  const choices = {
    local: [
      {
        id: "1",
        name: "Built-in",
        size: [1728, 1117],
        primary: true,
        cabledToBoth: false,
        inUse: true,
        canLeave: true,
      },
      {
        id: "2",
        name: "Dell",
        size: [2560, 1440],
        primary: false,
        cabledToBoth: true,
        inUse: false,
        canLeave: true,
      },
    ],
    peer: [
      {
        id: "3",
        name: "LG",
        size: [2560, 1440],
        primary: true,
        cabledToBoth: false,
        inUse: true,
        canLeave: false,
      },
    ],
  };
  const members = {
    order: ["local", "peer"],
    byKey: {
      local: { label: "This Mac", tone: "local" },
      peer: { label: "Windows PC", tone: "peer" },
    },
  };
  const all = displaySections({ choices, members, selected: "2" });
  assert.deepEqual(
    all.map((section) => [
      section.key,
      section.label,
      section.rows.map((row) => [row.id, row.selected]),
    ]),
    [
      [
        "local",
        "This Mac",
        [
          ["1", false],
          ["2", true],
        ],
      ],
      ["peer", "Windows PC", [["3", false]]],
    ],
  );
  assert.equal(all[0].rows[0].detail, "1728 × 1117 · Primary");
  assert.equal(all[0].rows[1].detail, "2560 × 1440 · Cabled to both");
  assert.deepEqual([all[0].rows[1].inUse, all[0].rows[1].locked], [false, false]);
  assert.equal(all[1].rows[0].detail, "2560 × 1440 · Primary · Its computer's only display");
  assert.deepEqual(all[1].rows[0].details, [
    "2560 × 1440",
    "Primary",
    "Its computer's only display",
  ]);
  assert.equal(all[1].rows[0].locked, true);
  const scoped = displaySections({ choices, members, scope: "peer" });
  assert.deepEqual(
    scoped.map((section) => section.key),
    ["peer"],
  );
  assert.equal(displayOwner(choices, "3"), "peer");
  assert.equal(displayOwner(choices, "9"), null);
});
