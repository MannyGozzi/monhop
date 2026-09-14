import assert from "node:assert/strict";
import test from "node:test";
import {
  arrangementGeometry,
  constrainTransform,
  describeArrangement,
  displayGroups,
  drawnDisplays,
  fitTransform,
  groupedPlacement,
  hiddenDisplays,
  hiddenFromLayout,
  ownSeams,
  innerBox,
  labelPlacement,
  layoutArrangement,
  movePlacement,
  placeGroup,
  placementFromCrossings,
  placementFromLayout,
  placementOffset,
  resolvePlacement,
  sameTransform,
  seamCrossings,
  sharedMonitors,
  sideRects,
  snapPlacement,
  tileRects,
  tiles,
  toFree,
  toGrouped,
  transformFits,
  truncateToWidth,
} from "./arrangement-model.mjs";

const monitor = (id, x, y, width, height, primary = false) => ({
  id,
  name: `Monitor ${id}`,
  origin: [x, y],
  size: [width, height],
  primary,
});
const groups = () =>
  displayGroups(
    [monitor("18446744073709551615", -100, -50, 200, 100, true), monitor("2", 0, 50, 100, 100)],
    [monitor("3", 0, 0, 100, 100)],
  );
const grouped = (g, offset) => groupedPlacement(g, offset);
const geometryAt = (g, offset) => arrangementGeometry(g, grouped(g, offset));
const all = (g, placement) => tiles(g, placement);
const measure = (value) => Array.from(value).length * 10;
const link = (from, to) => ({
  fromDisplay: from,
  fromEdge: "right",
  fromSpan: [0, 1],
  toDisplay: to,
  toEdge: "left",
  toSpan: [0, 1],
  hysteresis: 1,
});

test("computer groups preserve OS offsets, dimensions and full display identifiers", () => {
  const g = groups();
  assert.equal(g.source.displays[0].id, "18446744073709551615");
  assert.deepEqual(
    g.source.displays.map((d) => [d.x, d.y, d.width, d.height]),
    [
      [0, 0, 200, 100],
      [100, 100, 100, 100],
    ],
  );
  assert.deepEqual([g.source.width, g.source.height], [200, 200]);
  const saved = structuredClone(g);
  geometryAt(g, [200, 50]);
  assert.deepEqual(g, saved);
});

test("a grouped placement is each computer's own layout with one translation between them", () => {
  const g = groups();
  const placement = grouped(g, [200, 50]);
  assert.deepEqual(placement.positions, {
    "18446744073709551615": [0, 0],
    2: [100, 100],
    3: [200, 50],
  });
  assert.deepEqual(placementOffset(g, placement), [200, 50]);
  assert.deepEqual(
    placementOffset(g, movePlacement(g, placement, { side: "source" }, [-30, 7])),
    [230, 43],
  );
  assert.equal(placementOffset(g, movePlacement(g, placement, { id: "2" }, [0, 1])), null);
  assert.equal(groupedPlacement(g, [NaN, 0]), null);
});

test("one physical seam can cross two stacked monitors without stretching either edge", () => {
  const result = geometryAt(groups(), [200, 50]);
  assert.equal(result.connected, true);
  assert.equal(result.seams.length, 2);
  assert.deepEqual(
    result.seams.map((s) => [s.fromSpan, s.toSpan]),
    [
      [
        [0.5, 1],
        [0, 0.5],
      ],
      [
        [0, 0.5],
        [0.5, 1],
      ],
    ],
  );
  assert.deepEqual(
    result.seams.map((s) => [s.start, s.end]),
    [
      [
        [200, 50],
        [200, 100],
      ],
      [
        [200, 100],
        [200, 150],
      ],
    ],
  );
  assert.deepEqual(
    placementFromCrossings(groups(), seamCrossings(result.seams)),
    grouped(groups(), [200, 50]),
  );
});

test("gaps, overlap, corner-only contact and non-finite positions never authorize a seam", () => {
  const g = groups();
  for (const offset of [
    [201, 50],
    [190, 50],
    [200, 200],
    [NaN, 50],
    [Infinity, 0],
    [21_000_000, 0],
  ])
    assert.equal(geometryAt(g, offset).connected, false);
});

test("snap only within the requested distance, without snapping through another monitor", () => {
  const g = groups();
  const snapped = (offset, distance) =>
    placementOffset(g, snapPlacement(g, grouped(g, offset), { side: "destination" }, distance));
  assert.deepEqual(snapped([206, 50], 8), [200, 50]);
  assert.deepEqual(snapped([220, 50], 8), [220, 50]);
  assert.deepEqual(snapped([201, 200], 8), [201, 200]);
});

test("all four quick placements produce real opposite-edge contacts", () => {
  for (const side of ["left", "right", "top", "bottom"]) {
    const pose = placeGroup(groups(), side);
    const geometry = arrangementGeometry(groups(), pose);
    assert.equal(geometry.connected, true, side);
    assert.ok(
      geometry.seams.some((s) => s.fromEdge === side),
      side,
    );
  }
  assert.equal(placeGroup(groups(), "__proto__"), null);
});

test("saved translations reconstruct exactly and legacy stretched edges are not misrepresented", () => {
  const g = displayGroups([monitor("1", 0, 0, 2560, 1440)], [monitor("2", 0, 0, 1728, 1117)]);
  const old = [
    {
      fromDisplay: "1",
      toDisplay: "2",
      fromEdge: "right",
      toEdge: "left",
      fromSpan: [0, 1],
      toSpan: [0, 1],
    },
  ];
  assert.equal(placementFromCrossings(g, old), null);
  const actual = geometryAt(g, [2560, 78.5]);
  assert.deepEqual(
    placementFromCrossings(g, seamCrossings(actual.seams)),
    grouped(g, [2560, 78.5]),
  );
  const tampered = seamCrossings(actual.seams);
  tampered[0].toSpan[1] = 0.5;
  assert.equal(placementFromCrossings(g, tampered), null);
});

test("saved positions restore any placement, and are checked against the saved crossings and displays", () => {
  const g = groups();
  const free = movePlacement(g, toFree(grouped(g, [200, 50])), { id: "2" }, [0, 100]);
  const geometry = arrangementGeometry(g, free);
  assert.equal(geometry.connected, true);
  const saved = layoutArrangement(free);
  assert.deepEqual(saved, {
    mode: "free",
    positions: [
      { display: "18446744073709551615", x: 0, y: 0 },
      { display: "2", x: 100, y: 200 },
      { display: "3", x: 200, y: 50 },
    ],
    hidden: [],
  });
  assert.deepEqual(placementFromLayout(g, saved, seamCrossings(geometry.seams)), free);
  assert.equal(placementFromLayout(g, saved, seamCrossings(geometryAt(g, [200, 50]).seams)), null);
  assert.equal(
    placementFromLayout(g, { mode: "free", positions: saved.positions.slice(1) }, null),
    null,
  );
  assert.equal(
    placementFromLayout(
      g,
      { mode: "free", positions: [...saved.positions, { display: "9", x: 0, y: 0 }] },
      null,
    ),
    null,
  );
  // Grouped saves that omit positions still come back from the crossings alone.
  const crossings = seamCrossings(geometryAt(g, [200, 50]).seams);
  assert.deepEqual(
    placementFromLayout(g, { mode: "grouped", positions: [] }, crossings),
    grouped(g, [200, 50]),
  );
  assert.deepEqual(placementFromLayout(g, null, crossings), grouped(g, [200, 50]));
  assert.equal(
    placementFromLayout(g, { mode: "grouped", positions: saved.positions }, crossings),
    null,
  );
});

test("mirrored or overlapping monitors stay visible but cannot create ambiguous routes", () => {
  const g = displayGroups(
    [monitor("1", 0, 0, 100, 100), monitor("2", 0, 0, 100, 100)],
    [monitor("3", 0, 0, 100, 100)],
  );
  const view = geometryAt(g, [100, 0]);
  assert.equal(view.groups.source.displays.length, 2);
  assert.equal(view.valid, false);
  assert.match(view.message, /mirrored|overlapping/i);
  // The editor still draws them, so the message points at something on screen.
  assert.deepEqual(
    view.tiles.map((t) => t.id),
    ["1", "2", "3"],
  );
});

test("a grouped drop never keeps an overlap: it resolves to the nearest touching placement or to nothing", () => {
  const g = groups();
  const resolved = (offset) => {
    const p = resolvePlacement(g, grouped(g, offset), { side: "destination" });
    return p && placementOffset(g, p);
  };
  assert.deepEqual(resolved([200, 50]), [200, 50]);
  assert.equal(geometryAt(g, [150, 50]).valid, false);
  assert.deepEqual(resolved([150, 50]), [200, 50]);
  assert.deepEqual(resolved([260, 50]), [200, 50]);
  assert.deepEqual(resolved([0, -140]), [0, -100]);
  for (const offset of [
    [NaN, 0],
    [Infinity, 0],
    [21_000_000, 0],
  ])
    assert.equal(resolvePlacement(g, grouped(g, offset), { side: "destination" }), null);
  assert.equal(resolvePlacement(null, null, { side: "destination" }), null);
});

test("every resolved grouped drop is a valid connected arrangement, wherever it is dropped", () => {
  const g = groups();
  for (let x = -260; x <= 260; x += 37) {
    for (let y = -260; y <= 260; y += 41) {
      const resolved = resolvePlacement(g, grouped(g, [x, y]), { side: "source" });
      assert.ok(resolved, `${x},${y}`);
      const geometry = arrangementGeometry(g, resolved);
      assert.equal(geometry.valid, true, `${x},${y}`);
      assert.equal(geometry.connected, true, `${x},${y}`);
    }
  }
});

test("a free drop keeps displays apart but may leave the computers unconnected", () => {
  const g = groups();
  const start = toFree(grouped(g, [200, 50]));
  const away = movePlacement(g, start, { id: "3" }, [300, 0]);
  const geometry = arrangementGeometry(g, away);
  assert.equal(geometry.valid, true);
  assert.equal(geometry.connected, false);
  assert.match(geometry.message, /touches one from the other computer/);
  assert.deepEqual(resolvePlacement(g, away, { id: "3" }), away);
  const overlapping = movePlacement(g, start, { id: "3" }, [-50, 0]);
  assert.equal(arrangementGeometry(g, overlapping).valid, false);
  const resolved = resolvePlacement(g, overlapping, { id: "3" });
  assert.equal(arrangementGeometry(g, resolved).valid, true);
  assert.deepEqual(resolved.positions[3], [200, 50]);
  // The moving display may not push a display of its own computer into another one either.
  const ownOverlap = movePlacement(g, start, { id: "2" }, [0, -60]);
  assert.equal(arrangementGeometry(g, ownOverlap).valid, false);
  assert.equal(arrangementGeometry(g, resolvePlacement(g, ownOverlap, { id: "2" })).valid, true);
});

test("free snapping aligns to any nearby edge, including a display of the same computer", () => {
  const g = groups();
  const start = toFree(grouped(g, [200, 50]));
  const nudged = movePlacement(g, start, { id: "2" }, [0, 6]);
  assert.deepEqual(snapPlacement(g, nudged, { id: "2" }, 8).positions[2], [100, 100]);
  assert.deepEqual(snapPlacement(g, nudged, { id: "2" }, 4).positions[2], [100, 106]);
  const near = movePlacement(g, start, { id: "3" }, [5, 0]);
  assert.deepEqual(snapPlacement(g, near, { id: "3" }, 8).positions[3], [200, 50]);
});

test("switching modes keeps the picture: free starts where grouped was, grouped returns each computer's layout", () => {
  const g = groups();
  const placement = grouped(g, [200, 50]);
  const free = toFree(placement);
  assert.equal(free.mode, "free");
  assert.deepEqual(free.positions, placement.positions);
  assert.deepEqual(toGrouped(g, free), placement);
  const edited = movePlacement(g, free, { id: "2" }, [0, 100]);
  const back = toGrouped(g, edited);
  assert.equal(back.mode, "grouped");
  assert.ok(placementOffset(g, back));
  assert.equal(arrangementGeometry(g, back).connected, true);
});

test("fit frames the whole arrangement and reports a view that no longer frames it", () => {
  const stage = { width: 400, height: 220 };
  const g = groups();
  const near = all(g, grouped(g, [200, 50]));
  const far = all(g, grouped(g, [4000, 50]));
  const fitted = fitTransform(near, stage);
  assert.equal(transformFits(near, fitted, stage), true);
  assert.equal(transformFits(far, fitted, stage), false);
  assert.equal(transformFits(far, fitTransform(far, stage), stage), true);
  assert.equal(sameTransform(fitted, fitTransform(near, stage)), true);
  assert.equal(sameTransform(fitted, fitTransform(far, stage)), false);
  const box = innerBox(stage);
  for (const rect of [
    ...Object.values(tileRects(near, fitted)),
    ...Object.values(sideRects(near, fitted)),
  ]) {
    assert.ok(rect.x >= box.x - 1 && rect.x + rect.width <= box.x + box.width + 1);
    assert.ok(rect.y >= box.y - 1 && rect.y + rect.height <= box.y + box.height + 1);
  }
});

test("group labels stay on the canvas, off the other computer and off each other", () => {
  const stage = { width: 420, height: 200 };
  const size = { width: 130, height: 16 };
  const g = displayGroups([monitor("1", 0, 0, 200, 100, true)], [monitor("2", 0, 0, 200, 100)]);
  for (const offset of [
    [200, 0],
    [-200, 0],
    [0, 100],
    [0, -100],
  ]) {
    const placed = all(g, grouped(g, offset));
    const rects = sideRects(placed, fitTransform(placed, stage));
    const first = { ...labelPlacement(rects.source, rects.destination, stage, size), ...size };
    const second = {
      ...labelPlacement(rects.destination, rects.source, stage, size, undefined, [first]),
      ...size,
    };
    const where = JSON.stringify(offset);
    for (const [label, other] of [
      [first, rects.destination],
      [second, rects.source],
    ]) {
      assert.notEqual(label.placement, "inside", where);
      assert.ok(label.x >= 0 && label.x + size.width <= stage.width, where);
      assert.ok(label.y >= 0 && label.y + size.height <= stage.height, where);
      assert.equal(boxesOverlap(label, other), false, where);
    }
    assert.equal(boxesOverlap(first, second), false, where);
  }
});

test("names are shortened by measured width, never mid-character, and vanish before they spill", () => {
  assert.equal(truncateToWidth("Built-in Retina Display", 300, measure), "Built-in Retina Display");
  assert.equal(truncateToWidth("Built-in Retina Display", 55, measure), "Buil…");
  assert.equal(truncateToWidth("Built-in Retina Display", 5, measure), "");
  assert.equal(truncateToWidth("🖥🖥🖥 Studio", 25, measure), "🖥…");
  assert.equal(truncateToWidth("", 100, measure), "");
  assert.equal(truncateToWidth("Name", 0, measure), "");
});

test("the crossing is described in words, and an unconnected arrangement says why", () => {
  const connected = geometryAt(groups(), [200, 50]);
  assert.match(
    describeArrangement(connected),
    /crosses on 2 edges, starting at the right edge of Monitor 1844/,
  );
  const single = geometryAt(
    displayGroups([monitor("1", 0, 0, 200, 100, true)], [monitor("2", 0, 0, 200, 100)]),
    [200, 0],
  );
  assert.equal(
    describeArrangement(single),
    "The pointer crosses on the right edge of Monitor 1, into Monitor 2.",
  );
  assert.match(describeArrangement(geometryAt(groups(), [400, 50])), /touch/i);
});

function boxesOverlap(a, b) {
  return (
    Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x) > 0 &&
    Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y) > 0
  );
}

test("an edit pans the kept view instead of rescaling it, and fit restores the ideal frame", () => {
  const stage = { width: 600, height: 600 };
  const g = groups();
  const near = all(g, grouped(g, [200, 50]));
  const moved = all(g, grouped(g, [200, 150]));
  const fitted = fitTransform(near, stage);
  const panned = constrainTransform(moved, fitted, stage);
  assert.equal(panned.scale, fitted.scale);
  assert.equal(transformFits(moved, panned, stage), true);
  assert.equal(sameTransform(panned, fitted), false);
  assert.equal(sameTransform(panned, fitTransform(moved, stage)), false);
  assert.equal(constrainTransform(near, fitted, stage).originY, fitted.originY);
  assert.equal(constrainTransform(all(g, grouped(g, [4000, 50])), fitted, stage), null);
  assert.equal(constrainTransform(near, null, stage), null);
});

const shared = () => {
  const key = "10ac-4123-0000abcd";
  return {
    key,
    source: [
      monitor("1", 0, 0, 200, 100, true),
      { ...monitor("2", 200, 0, 100, 100), monitor: key },
    ],
    destination: [
      monitor("3", 0, 0, 100, 100, true),
      { ...monitor("4", 100, 0, 100, 100), monitor: key },
    ],
  };
};

test("a monitor cabled to both computers is drawn once, on the input computer's side unless marked otherwise", () => {
  const { key, source, destination } = shared();
  const pairs = sharedMonitors(source, destination);
  assert.deepEqual(
    pairs.map((p) => [p.monitor, p.source.id, p.destination.id]),
    [[key, "2", "4"]],
  );
  assert.deepEqual(hiddenDisplays(source, destination), ["4"]);
  assert.deepEqual(hiddenDisplays(source, destination, null), ["4"]);
  assert.deepEqual(hiddenDisplays(source, destination, ["2"]), ["2"]);
  // A list is exact: any display can be marked not in use, both copies can, and an unknown id is nothing.
  assert.deepEqual(hiddenDisplays(source, destination, []), []);
  assert.deepEqual(hiddenDisplays(source, destination, ["9"]), []);
  assert.deepEqual(hiddenDisplays(source, destination, ["2", "4"]), ["2", "4"]);
  assert.deepEqual(hiddenDisplays(source, destination, ["1", "4"]), ["1", "4"]);
  // A computer never loses its last display, and two single-display computers both keep theirs.
  assert.deepEqual(hiddenDisplays(source, destination, ["2", "1"]), ["1"]);
  assert.deepEqual(hiddenDisplays(source, [destination[1]]), ["2"]);
  assert.deepEqual(hiddenDisplays(source, [destination[1]], ["4"]), []);
  assert.deepEqual(hiddenDisplays([source[1]], [destination[1]]), []);
  // A key that repeats on one side identifies nothing; a missing key never matches.
  assert.deepEqual(
    sharedMonitors([...source, { ...monitor("5", 0, 100, 100, 100), monitor: key }], destination),
    [],
  );
  assert.deepEqual(sharedMonitors(source, [monitor("6", 0, 0, 100, 100)]), []);
  assert.deepEqual(
    drawnDisplays(source, ["2"], pairs).map((d) => [d.id, d.shared === true]),
    [["1", false]],
  );
  assert.deepEqual(
    drawnDisplays(destination, ["2"], pairs).map((d) => [d.id, d.shared === true]),
    [
      ["3", false],
      ["4", true],
    ],
  );
  const drawnGroups = displayGroups(
    drawnDisplays(source, ["2"], pairs),
    drawnDisplays(destination, ["2"], pairs),
  );
  assert.deepEqual(
    drawnGroups.destination.displays.map((d) => [d.id, d.shared, d.monitor]),
    [
      ["3", false, null],
      ["4", true, key],
    ],
  );
  assert.equal(drawnGroups.source.width, 200);
});

test("a stored layout lists what it left out, and an older one shows the copy it used through its links", () => {
  const { source, destination } = shared();
  assert.deepEqual(
    hiddenFromLayout(
      { links: [link("1", "3")], arrangement: { mode: "free", positions: [], hidden: ["2"] } },
      source,
      destination,
    ),
    ["2"],
  );
  // An empty list is a choice too: every display in use.
  assert.deepEqual(
    hiddenFromLayout(
      { links: [link("1", "3")], arrangement: { mode: "grouped", positions: [], hidden: [] } },
      source,
      destination,
    ),
    [],
  );
  assert.deepEqual(
    hiddenFromLayout({ links: [link("1", "4"), link("4", "1")] }, source, destination),
    ["2"],
  );
  assert.deepEqual(
    hiddenFromLayout({ links: [link("2", "3"), link("3", "2")] }, source, destination),
    ["4"],
  );
  assert.deepEqual(hiddenFromLayout({ links: [link("1", "3")] }, source, destination), ["4"]);
  assert.deepEqual(hiddenFromLayout({ links: [link("2", "4")] }, source, destination), ["4"]);
  assert.deepEqual(
    hiddenFromLayout({ sourceDisplay: "2", links: [link("1", "3")] }, source, destination),
    ["4"],
  );
  assert.deepEqual(layoutArrangement({ mode: "free", positions: { 1: [0, 0] } }, ["4", "2"]), {
    mode: "free",
    positions: [{ display: "1", x: 0, y: 0 }],
    hidden: ["2", "4"],
  });
  assert.deepEqual(layoutArrangement({ mode: "grouped", positions: {} }), {
    mode: "grouped",
    positions: [],
    hidden: [],
  });
});

test("own seams mirror the native edge inheritance", () => {
  const a = { id: "1", name: "A", origin: [-100, -100], size: [200, 100] };
  const b = { id: "2", name: "B", origin: [0, 0], size: [100, 100] };
  assert.deepEqual(
    ownSeams([a, b]).map((s) => [
      s.fromDisplay,
      s.fromEdge,
      s.fromSpan,
      s.toDisplay,
      s.toEdge,
      s.toSpan,
    ]),
    [
      ["1", "bottom", [0.5, 1], "2", "top", [0, 1]],
      ["2", "top", [0, 1], "1", "bottom", [0.5, 1]],
    ],
  );
  assert.deepEqual(ownSeams([a, b], ["2"]), []);
  assert.deepEqual(ownSeams([a, { ...b, origin: [0, 1] }]), []);
});
