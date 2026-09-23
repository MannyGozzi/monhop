import assert from "node:assert/strict";
import test from "node:test";

import { placementOffset } from "./arrangement-model.mjs";
import {
  arrangementMotion,
  dashboardCaption,
  savedDashboardArrangement,
} from "./dashboard-arrangement.mjs";

const edges = {
  left: ["right", [-100, 0]],
  right: ["left", [100, 0]],
  top: ["bottom", [0, -100]],
  bottom: ["top", [0, 100]],
};

function setup(edge = "right") {
  const [destinationEdge] = edges[edge];
  return {
    saved: true,
    localDisplays: [
      { id: "1", name: "This display", origin: [-100, -50], size: [100, 100], primary: true },
    ],
    peerDisplays: [
      { id: "2", name: "Saved display", origin: [0, 0], size: [100, 100], primary: true },
    ],
    previewLayout: {
      links: [
        {
          fromDisplay: "1",
          fromEdge: edge,
          fromSpan: [0, 1],
          toDisplay: "2",
          toEdge: destinationEdge,
          toSpan: [0, 1],
          hysteresis: 1,
        },
        {
          fromDisplay: "2",
          fromEdge: destinationEdge,
          fromSpan: [0, 1],
          toDisplay: "1",
          toEdge: edge,
          toSpan: [0, 1],
          hysteresis: 1,
        },
      ],
    },
  };
}

test("saved dashboard arrangements preserve the crossing edge and offset", () => {
  for (const [edge, [, offset]] of Object.entries(edges)) {
    const value = setup(edge);
    const result = savedDashboardArrangement(value);
    assert.equal(result.available, true, edge);
    assert.deepEqual(placementOffset(result.groups, result.placement), offset, edge);
    assert.equal(result.seams.length, 1, edge);
    assert.equal(result.seams[0].fromEdge, edge, edge);
  }
});

test("saved preview keeps each computer's monitor geometry unchanged", () => {
  const value = setup();
  value.localDisplays.push({
    id: "3",
    name: "This side display",
    origin: [0, 50],
    size: [100, 100],
    primary: false,
  });
  value.previewLayout = {
    links: [
      {
        fromDisplay: "3",
        fromEdge: "right",
        fromSpan: [0, 1],
        toDisplay: "2",
        toEdge: "left",
        toSpan: [0, 1],
        hysteresis: 1,
      },
      {
        fromDisplay: "2",
        fromEdge: "left",
        fromSpan: [0, 1],
        toDisplay: "3",
        toEdge: "right",
        toSpan: [0, 1],
        hysteresis: 1,
      },
    ],
  };
  const before = structuredClone(value);
  const result = savedDashboardArrangement(value);
  assert.equal(result.available, true);
  assert.deepEqual(value, before);
  assert.deepEqual(
    result.groups.local.displays.map((display) => [
      display.id,
      display.x,
      display.y,
      display.width,
      display.height,
    ]),
    [
      ["1", 0, 0, 100, 100],
      ["3", 100, 100, 100, 100],
    ],
  );
  assert.deepEqual([result.groups.local.width, result.groups.local.height], [200, 200]);
});

test("saved positions are drawn where they were applied, and a stale save falls back to the crossings", () => {
  const value = setup();
  value.previewLayout.arrangement = {
    positions: [
      { display: "1", x: 0, y: 0 },
      { display: "2", x: 100, y: 0 },
    ],
  };
  const result = savedDashboardArrangement(value);
  assert.equal(result.available, true);
  assert.deepEqual(
    result.tiles.map((tile) => [tile.id, tile.x, tile.y]),
    [
      ["1", 0, 0],
      ["2", 100, 0],
    ],
  );
  assert.equal(result.seams.length, 1);

  // A position naming a display that no longer exists cannot cover every connected display, so the
  // translation is rebuilt from the crossings instead of trusting the stale positions.
  const stale = structuredClone(value);
  stale.previewLayout.arrangement.positions.push({ display: "9", x: 0, y: 300 });
  const fallback = savedDashboardArrangement(stale);
  assert.equal(fallback.available, true);
  assert.deepEqual(
    fallback.tiles.map((tile) => [tile.id, tile.x, tile.y]),
    [
      ["1", 0, 0],
      ["2", 100, 0],
    ],
  );
});

test("missing, stale, or malformed saved details get an honest unavailable preview", () => {
  const valid = setup();
  const cases = [
    { ...valid, previewLayout: null },
    // An odd link count has no reciprocal half.
    { ...valid, previewLayout: { links: [valid.previewLayout.links[0]] } },
    // Spans that no longer mirror each other are not a real reciprocal pair.
    {
      ...valid,
      previewLayout: {
        links: valid.previewLayout.links.map((link) => ({ ...link, fromSpan: [0, 0.5] })),
      },
    },
    // A link naming a display that does not exist breaks the reciprocal match.
    {
      ...valid,
      previewLayout: {
        links: [
          { ...valid.previewLayout.links[0], fromDisplay: "9" },
          valid.previewLayout.links[1],
        ],
      },
    },
    { ...valid, peerDisplays: [{ ...valid.peerDisplays[0], id: "1" }] },
  ];
  for (const value of cases) {
    const result = savedDashboardArrangement(value);
    assert.equal(result.available, false);
    assert.match(result.message, /saved|review|valid/i);
  }
});

test("a layout with no crossing yet still draws both computers' displays, just unconnected", () => {
  const value = setup();
  value.previewLayout = { links: [] };
  const result = savedDashboardArrangement(value);
  assert.equal(result.available, true);
  assert.equal(result.noCrossingYet, true);
  assert.deepEqual(result.seams, []);
  assert.deepEqual(result.tiles.map((tile) => tile.id).toSorted(), ["1", "2"]);
  // The caption says what is missing rather than warning that the saved details need a review.
  assert.equal(dashboardCaption(result), "No crossing yet. Arrange the displays to connect them.");
  assert.equal(
    dashboardCaption(savedDashboardArrangement(setup())),
    "Saved display positions. Not a current display check.",
  );
});

test("a redrawn viewport knows which displays moved, and which ones are new on screen", () => {
  const before = { 1: { x: 0, y: 0 }, 2: { x: 100, y: 0 } };
  const after = { 1: { x: 0, y: 0.2 }, 2: { x: 140, y: 30 }, 3: { x: 260, y: 0 } };
  const motion = arrangementMotion(before, after);
  // A display that stayed put, give or take a rounding difference, is left alone.
  assert.deepEqual(Object.keys(motion.moved), ["2"]);
  // The delta is where it was minus where it now is: the offset the move starts from.
  assert.deepEqual(motion.moved["2"], [-40, -30]);
  assert.deepEqual(motion.entered, ["3"]);
  // The first drawing has everything arriving and nothing moving.
  assert.deepEqual(arrangementMotion(undefined, after), { moved: {}, entered: ["1", "2", "3"] });
  // A display that vanished left with its node, so it is neither moved nor entering.
  assert.deepEqual(arrangementMotion(before, {}), { moved: {}, entered: [] });
});

test("the dashboard preview and the editor draw through one shared renderer", async () => {
  const { readFile } = await import("node:fs/promises");
  for (const file of ["dashboard-arrangement.mjs", "arrangement-view.mjs"]) {
    const source = await readFile(new URL(file, import.meta.url), "utf8");
    assert.match(source, /from "\.\/arrangement-render\.mjs"/, file);
    assert.doesNotMatch(source, /createElementNS/, file);
  }
});

test("a monitor cabled to both computers is previewed once, on the local copy unless marked otherwise", () => {
  const key = "10ac-4123-0000abcd";
  const saved = setup("right");
  saved.localDisplays = [
    saved.localDisplays[0],
    { id: "3", name: "Desk", origin: [-100, 50], size: [100, 100], primary: false, monitor: key },
  ];
  saved.peerDisplays = [
    saved.peerDisplays[0],
    { id: "4", name: "Desk", origin: [100, 0], size: [100, 100], primary: false, monitor: key },
  ];
  // No hidden ids saved and the crossing never touches the shared monitor: this computer keeps its
  // own copy by default and the peer's is left out of the picture.
  let preview = savedDashboardArrangement(saved);
  assert.equal(preview.available, true);
  assert.deepEqual(preview.tiles.map((tile) => [tile.id, tile.side, tile.shared]).toSorted(), [
    ["1", "local", false],
    ["2", "peer", false],
    ["3", "local", true],
  ]);
  // Saved positions that hide this computer's copy draw the peer's instead.
  saved.previewLayout.arrangement = {
    positions: [
      { display: "1", x: 0, y: 0 },
      { display: "2", x: 100, y: 0 },
      { display: "4", x: 200, y: 0 },
    ],
    hidden: ["3"],
  };
  preview = savedDashboardArrangement(saved);
  assert.equal(preview.available, true);
  assert.deepEqual(preview.tiles.map((tile) => [tile.id, tile.side, tile.shared]).toSorted(), [
    ["1", "local", false],
    ["2", "peer", false],
    ["4", "peer", true],
  ]);
  assert.deepEqual(preview.placement.positions["4"], [200, 0]);
});
