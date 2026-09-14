import assert from "node:assert/strict";
import test from "node:test";

import { placementOffset } from "./arrangement-model.mjs";
import { savedDashboardArrangement } from "./dashboard-arrangement.mjs";

const edges = {
  left: ["right", [-100, 0]],
  right: ["left", [100, 0]],
  top: ["bottom", [0, -100]],
  bottom: ["top", [0, 100]],
};

function setup(sourceSide = "local", edge = "right") {
  const sourceId = sourceSide === "local" ? "1" : "2";
  const destinationId = sourceSide === "local" ? "2" : "1";
  const [destinationEdge] = edges[edge];
  return {
    saved: true,
    sourceSide,
    localDisplays: [
      { id: "1", name: "This display", origin: [-100, -50], size: [100, 100], primary: true },
    ],
    peerDisplays: [
      { id: "2", name: "Saved display", origin: [0, 0], size: [100, 100], primary: true },
    ],
    previewLayout: {
      sourceDisplay: sourceId,
      links: [
        {
          fromDisplay: sourceId,
          fromEdge: edge,
          fromSpan: [0, 1],
          toDisplay: destinationId,
          toEdge: destinationEdge,
          toSpan: [0, 1],
          hysteresis: 1,
        },
        {
          fromDisplay: destinationId,
          fromEdge: destinationEdge,
          fromSpan: [0, 1],
          toDisplay: sourceId,
          toEdge: edge,
          toSpan: [0, 1],
          hysteresis: 1,
        },
      ],
    },
  };
}

test("saved dashboard arrangements preserve each source direction and highlighted seam", () => {
  for (const sourceSide of ["local", "peer"]) {
    for (const [edge, [, offset]] of Object.entries(edges)) {
      const value = setup(sourceSide, edge);
      const result = savedDashboardArrangement(value);
      assert.equal(result.available, true, `${sourceSide} ${edge}`);
      assert.equal(result.sourceSide, sourceSide, `${sourceSide} ${edge}`);
      assert.equal(result.placement.mode, "grouped", `${sourceSide} ${edge}`);
      assert.deepEqual(
        placementOffset(result.groups, result.placement),
        offset,
        `${sourceSide} ${edge}`,
      );
      assert.equal(result.seams.length, 1, `${sourceSide} ${edge}`);
      assert.equal(result.seams[0].fromEdge, edge, `${sourceSide} ${edge}`);
    }
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
    sourceDisplay: "3",
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
    result.groups.source.displays.map((display) => [
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
  assert.deepEqual([result.groups.source.width, result.groups.source.height], [200, 200]);
});

test("saved free positions are drawn where they were applied", () => {
  const value = setup();
  value.localDisplays.push({
    id: "3",
    name: "This side display",
    origin: [0, 50],
    size: [100, 100],
    primary: false,
  });
  // Display 3 was pulled level with display 1, which its own system layout does not do.
  value.previewLayout = {
    sourceDisplay: "1",
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
    arrangement: {
      mode: "free",
      positions: [
        { display: "1", x: 0, y: 0 },
        { display: "2", x: 200, y: 0 },
        { display: "3", x: 100, y: 0 },
      ],
    },
  };
  const result = savedDashboardArrangement(value);
  assert.equal(result.available, true);
  assert.equal(result.placement.mode, "free");
  assert.deepEqual(
    result.tiles.map((tile) => [tile.id, tile.x, tile.y]),
    [
      ["1", 0, 0],
      ["3", 100, 0],
      ["2", 200, 0],
    ],
  );
  assert.equal(result.seams.length, 1);

  const stale = structuredClone(value);
  stale.previewLayout.arrangement.positions.push({ display: "9", x: 0, y: 300 });
  assert.equal(savedDashboardArrangement(stale).available, false);
});

test("missing, stale, or malformed saved details get an honest unavailable preview", () => {
  const valid = setup();
  const cases = [
    { ...valid, previewLayout: null },
    { ...valid, previewLayout: { ...valid.previewLayout, links: [] } },
    { ...valid, previewLayout: { ...valid.previewLayout, sourceDisplay: "9" } },
    { ...valid, previewLayout: { ...valid.previewLayout, links: [valid.previewLayout.links[0]] } },
    {
      ...valid,
      previewLayout: {
        ...valid.previewLayout,
        links: valid.previewLayout.links.map((link) => ({ ...link, fromSpan: [0, 0.5] })),
      },
    },
    { ...valid, peerDisplays: [{ ...valid.peerDisplays[0], id: "1" }] },
  ];
  for (const value of cases) {
    const result = savedDashboardArrangement(value);
    assert.equal(result.available, false);
    assert.match(result.message, /saved|review/i);
  }
});

test("the dashboard preview and the editor draw through one shared renderer", async () => {
  const { readFile } = await import("node:fs/promises");
  for (const file of ["dashboard-arrangement.mjs", "arrangement-view.mjs"]) {
    const source = await readFile(new URL(file, import.meta.url), "utf8");
    assert.match(source, /from "\.\/arrangement-render\.mjs"/, file);
    assert.doesNotMatch(source, /createElementNS/, file);
  }
});

test("a monitor cabled to both computers is previewed once, on the side the saved layout uses", () => {
  const key = "10ac-4123-0000abcd";
  const saved = setup("local", "right");
  saved.localDisplays = [
    saved.localDisplays[0],
    { id: "3", name: "Desk", origin: [-100, 50], size: [100, 100], primary: false, monitor: key },
  ];
  saved.peerDisplays = [
    saved.peerDisplays[0],
    { id: "4", name: "Desk", origin: [100, 0], size: [100, 100], primary: false, monitor: key },
  ];
  // No hidden ids saved: the peer's copy stays out because the input computer keeps its own.
  let preview = savedDashboardArrangement(saved);
  assert.equal(preview.available, true);
  assert.deepEqual(preview.tiles.map((tile) => [tile.id, tile.side, tile.shared]).toSorted(), [
    ["1", "source", false],
    ["2", "destination", false],
    ["3", "source", true],
  ]);
  // Saved positions that hide this computer's copy draw the peer's instead.
  saved.previewLayout.arrangement = {
    mode: "grouped",
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
    ["1", "source", false],
    ["2", "destination", false],
    ["4", "destination", true],
  ]);
  assert.deepEqual(preview.placement.positions["4"], [200, 0]);
});
