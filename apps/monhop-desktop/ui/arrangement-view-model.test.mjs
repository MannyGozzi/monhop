import assert from "node:assert/strict";
import test from "node:test";
import {
  arrangementGeometry,
  chainPlacement,
  computerGroups,
  describeArrangement,
  fitTransform,
  groupRects,
  movePlacement,
  resolvePlacement,
} from "./arrangement-model.mjs";
import {
  COARSE_NUDGE_PIXELS,
  NUDGE_PIXELS,
  groupAriaText,
  membersFromOptions,
  nearestGroup,
  neighborWord,
  normalizeUseChoices,
  nudgeDirection,
  nudgePixels,
  nudgeTarget,
  tileAriaText,
} from "./arrangement-view-model.mjs";

const STAGE = { width: 400, height: 200 };
const display = (id, w = 100, h = 100) => ({
  id,
  name: `Monitor ${id}`,
  origin: [0, 0],
  size: [w, h],
});

// A chain of `keys.length` single-display computers, each placed touching the one before it, the
// same shape `chainPlacement` builds for a fresh arrangement of any size.
function chainOf(keys) {
  const groups = computerGroups(
    keys.map((key, index) => ({ key, displays: [display(`${index + 1}`)] })),
  );
  const placement = chainPlacement(groups, "right");
  const geometry = arrangementGeometry(groups, placement);
  const transform = fitTransform(geometry.tiles, STAGE);
  return { groups, geometry, rects: groupRects(geometry.tiles, transform) };
}

test("membersFromOptions: the legacy local/peer options produce today's defaults", () => {
  const members = membersFromOptions({});
  assert.deepEqual(members.order, ["local", "peer"]);
  assert.deepEqual(members.byKey.local, { label: "This computer", platform: null, tone: "local" });
  assert.deepEqual(members.byKey.peer, {
    label: "The other computer",
    platform: null,
    tone: "peer",
  });
});

test("membersFromOptions: legacy options carry through explicit labels and platforms", () => {
  const members = membersFromOptions({
    localLabel: "Mac mini",
    peerLabel: "Gaming PC",
    localPlatform: "macos",
    peerPlatform: "windows",
  });
  assert.equal(members.byKey.local.label, "Mac mini");
  assert.equal(members.byKey.peer.label, "Gaming PC");
  assert.equal(members.byKey.local.platform, "macos");
  assert.equal(members.byKey.peer.platform, "windows");
});

test("membersFromOptions: options.members names every member, defaulting label and tone", () => {
  const members = membersFromOptions({
    members: [
      { key: "aa", label: "Mac mini", platform: "macos", tone: "local" },
      { key: "bb", platform: "windows" },
      { key: "cc", platform: "windows" },
    ],
  });
  assert.deepEqual(members.order, ["aa", "bb", "cc"]);
  assert.equal(members.byKey.aa.label, "Mac mini");
  assert.equal(members.byKey.bb.label, "Computer 2");
  assert.equal(members.byKey.bb.tone, "peer");
  assert.equal(members.byKey.cc.tone, "peer-2");
});

test("membersFromOptions: a duplicate or missing key is rejected", () => {
  assert.equal(membersFromOptions({ members: [{ key: "a" }, { key: "a" }] }), null);
  assert.equal(membersFromOptions({ members: [{ key: "a" }, {}] }), null);
});

test("normalizeUseChoices: every member key gets a list, defaulting to empty", () => {
  const result = normalizeUseChoices(["local", "peer", "third"], {
    local: [1, 2],
    third: "not-array",
  });
  assert.deepEqual(result, { local: [1, 2], peer: [], third: [] });
});

test("neighborWord: two computers read exactly like the old two-computer wording", () => {
  const { rects } = chainOf(["local", "peer"]);
  assert.equal(neighborWord("local", rects), "to the left");
  assert.equal(neighborWord("peer", rects), "to the right");
});

test("neighborWord: a block touching two others compares against the nearer one", () => {
  const { rects } = chainOf(["west", "mid", "east"]);
  assert.equal(nearestGroup("mid", rects), "west");
  assert.equal(neighborWord("mid", rects), "to the right");
  assert.equal(neighborWord("east", rects), "to the right");
  assert.equal(neighborWord("west", rects), "to the left");
});

test("groupAriaText: the 2-member path produces today's exact string", () => {
  const { groups, geometry, rects } = chainOf(["local", "peer"]);
  const labels = { local: "This computer", peer: "The other computer" };
  const crossingText = describeArrangement(geometry);
  const local = groupAriaText({
    key: "local",
    tiles: geometry.tiles,
    groups,
    rects,
    labels,
    connected: geometry.connected,
    crossingText,
  });
  assert.equal(
    local,
    `This computer. 1 display, 100 × 100 together. Sits to the left of The other computer. ${crossingText} Drag, or use the arrow keys.`,
  );
  const peer = groupAriaText({
    key: "peer",
    tiles: geometry.tiles,
    groups,
    rects,
    labels,
    connected: geometry.connected,
    crossingText,
  });
  assert.equal(
    peer,
    `The other computer. 1 display, 100 × 100 together. Sits to the right of This computer. ${crossingText} Drag, or use the arrow keys.`,
  );
});

test("groupAriaText: aria text for 3 groups", () => {
  const { groups, geometry, rects } = chainOf(["west", "mid", "east"]);
  const labels = { west: "West", mid: "Mid", east: "East" };
  const crossingText = describeArrangement(geometry);
  const mid = groupAriaText({
    key: "mid",
    tiles: geometry.tiles,
    groups,
    rects,
    labels,
    connected: geometry.connected,
    crossingText,
  });
  assert.equal(
    mid,
    `Mid. 1 display, 100 × 100 together. Sits to the right of West. ${crossingText} Drag, or use the arrow keys.`,
  );
});

test("tileAriaText: 2-member wording says 'the other computer' when nothing touches", () => {
  const untouched = { id: "9", name: "Solo", width: 100, height: 100, primary: false };
  const text = tileAriaText({
    tile: untouched,
    groupLabel: "This computer",
    seams: [],
    otherGroupCount: 1,
  });
  assert.equal(
    text,
    "Solo, 100 × 100, on This computer. Not touching the other computer. Drag, or use the arrow keys.",
  );
});

test("tileAriaText: 3+ member wording says 'another computer' when nothing touches", () => {
  const untouched = { id: "9", name: "Solo", width: 100, height: 100, primary: true };
  const text = tileAriaText({ tile: untouched, groupLabel: "West", seams: [], otherGroupCount: 2 });
  assert.equal(
    text,
    "Solo, 100 × 100, primary display, on West. Not touching another computer. Drag, or use the arrow keys.",
  );
});

test("tileAriaText: a middle block reports both edges it crosses on", () => {
  const { groups, geometry } = chainOf(["west", "mid", "east"]);
  const midTile = geometry.tiles.find((t) => (t.group ?? t.side) === "mid");
  const text = tileAriaText({
    tile: midTile,
    groupLabel: "Mid",
    seams: geometry.seams,
    otherGroupCount: groups.order.length - 1,
  });
  assert.equal(
    text,
    `${midTile.name}, 100 × 100, on Mid. Crosses on its left and right edge. Drag, or use the arrow keys.`,
  );
});

test("nudgeDirection and nudgePixels map arrow keys, doubling with shift", () => {
  assert.deepEqual(nudgeDirection("ArrowRight"), [1, 0]);
  assert.equal(nudgeDirection("PageUp"), null);
  assert.deepEqual(nudgePixels("ArrowDown", false), [0, NUDGE_PIXELS]);
  assert.deepEqual(nudgePixels("ArrowUp", true), [0, -COARSE_NUDGE_PIXELS]);
  assert.equal(nudgePixels("Tab", false), null);
});

test("nudgeTarget: a nudge that stays legal keeps the block touching", () => {
  const { groups, geometry } = chainOf(["local", "peer"]);
  const transform = fitTransform(geometry.tiles, STAGE);
  // A small nudge down keeps the two blocks overlapping on the seam axis, so it stays legal.
  const target = nudgeTarget(
    groups,
    geometry.placement,
    { group: "peer" },
    "ArrowDown",
    false,
    transform.scale,
  );
  assert.ok(target);
  const after = arrangementGeometry(groups, target);
  assert.equal(after.connected, true);
});

test("nudgeTarget: a coarse (shift) nudge matches calling the model functions by hand", () => {
  const { groups, geometry } = chainOf(["west", "mid", "east"]);
  const transform = fitTransform(geometry.tiles, STAGE);
  const moving = { group: "east" };
  const target = nudgeTarget(
    groups,
    geometry.placement,
    moving,
    "ArrowRight",
    true,
    transform.scale,
  );
  const delta = [COARSE_NUDGE_PIXELS / transform.scale, 0];
  const expected = resolvePlacement(
    groups,
    movePlacement(groups, geometry.placement, moving, delta),
    moving,
  );
  assert.deepEqual(target, expected);
});

test("nudgeTarget: a moving group the model does not recognize has no legal target", () => {
  const { groups, geometry } = chainOf(["local", "peer"]);
  assert.equal(
    nudgeTarget(groups, geometry.placement, { group: "ghost" }, "ArrowRight", false, 1),
    null,
  );
});

test("nudgeTarget: an unrecognized key or non-finite scale yields no target", () => {
  const { groups, geometry } = chainOf(["local", "peer"]);
  assert.equal(nudgeTarget(groups, geometry.placement, { group: "peer" }, "Tab", false, 1), null);
  assert.equal(
    nudgeTarget(groups, geometry.placement, { group: "peer" }, "ArrowUp", false, 0),
    null,
  );
});
