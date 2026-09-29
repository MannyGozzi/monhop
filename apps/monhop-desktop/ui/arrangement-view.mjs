import {
  VIEW_INSETS,
  arrangementGeometry,
  constrainTransform,
  describeArrangement,
  fitTransform,
  formatSize,
  groupRects,
  movePlacement,
  movingIds,
  resolvePlacement,
  sameTransform,
  snapPlacement,
  tileRects,
  tiles,
} from "./arrangement-model.mjs";
import {
  createGroupLabelNode,
  createGroupNode,
  createGuideLayer,
  createLegend,
  createOutlineNode,
  createSeamLayer,
  labelBoxes,
  refineText,
  setPosition,
  svgNode,
} from "./arrangement-render.mjs";
import { revealPanel, setRevealOpen } from "./accordion.mjs";
import { switchRow } from "./dom.mjs";
import {
  groupAriaText,
  membersFromOptions,
  normalizeUseChoices,
  nudgePixels,
  nudgeTarget,
  tileAriaText,
} from "./arrangement-view-model.mjs";

const MIN_STAGE_WIDTH = 280;
const MIN_STAGE_HEIGHT = 190;
const SNAP_PIXELS = 14;
// Below this a group's rect moved a rounding difference, not a rearrangement worth FLIP-animating.
const GROUP_MOVE_EPSILON = 0.5;
const INSTRUCTIONS =
  "Drag a computer against another; the edge where they touch is where the pointer crosses. With a computer selected, arrow keys nudge it and Shift with an arrow moves it further.";

// Only one editor is mounted at a time, so the fitted view survives the rebuild a commit triggers.
let storedView = null;
let refitNext = true;

function shapeOf(value) {
  return `${value.tiles.map((t) => `${t.id}:${t.group ?? t.side}:${t.x}:${t.y}:${t.width}:${t.height}`).join(",")}`;
}

function focusKeyOf(node) {
  return node?.dataset?.focusKey ?? null;
}

function emptyPreview() {
  const layer = svgNode("g");
  layer.classList.add("arrangement-preview");
  return layer;
}

function reducedMotion() {
  return typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;
}

// What changed between two drawings' group rects: how far each continuing group moved (the inverse
// translate a FLIP transition starts from), which groups are on screen for the first time, and which
// ones are gone (their last node is handed to the leaving layer instead of being discarded).
function groupMotion(previous, current) {
  const moved = {};
  const entered = [];
  const left = [];
  const seen = new Set();
  for (const [key, rect] of Object.entries(current ?? {})) {
    seen.add(key);
    const was = previous?.[key];
    if (!was) {
      entered.push(key);
      continue;
    }
    const delta = [was.x - rect.x, was.y - rect.y];
    if (Math.abs(delta[0]) >= GROUP_MOVE_EPSILON || Math.abs(delta[1]) >= GROUP_MOVE_EPSILON)
      moved[key] = delta;
  }
  for (const key of Object.keys(previous ?? {})) if (!seen.has(key)) left.push(key);
  return { moved, entered, left };
}

function memberMaps(members) {
  const labels = {};
  const platforms = {};
  const tones = {};
  for (const key of members.order) {
    labels[key] = members.byKey[key].label;
    platforms[key] = members.byKey[key].platform;
    tones[key] = members.byKey[key].tone;
  }
  return { labels, platforms, tones };
}

export function createArrangementView(options) {
  let arrangement = options.arrangement;
  let shared = Array.isArray(options.shared) ? options.shared : [];
  let members = membersFromOptions(options) ?? membersFromOptions({});
  let { labels, platforms, tones } = memberMaps(members);
  let inUse = normalizeUseChoices(members.order, options.inUse);
  let disabled = options.disabled === true;
  let handlers = {
    onCommit: options.onCommit,
    onReset: options.onReset,
    onShowMonitor: options.onShowMonitor,
    onUseDisplay: options.onUseDisplay,
  };

  const root = document.createElement("section");
  root.className = "arrangement-editor";
  root.setAttribute("aria-labelledby", "arrangement-title");

  const heading = document.createElement("div");
  heading.className = "arrangement-heading";
  const title = document.createElement("h3");
  title.id = "arrangement-title";
  title.textContent = "Arrange displays";
  const status = document.createElement("p");
  status.className = "arrangement-status";
  status.setAttribute("aria-live", "polite");
  heading.append(title, status);

  const instructions = document.createElement("p");
  instructions.id = "arrangement-instructions";
  instructions.className = "arrangement-instructions";
  instructions.textContent = INSTRUCTIONS;

  const stage = document.createElement("div");
  stage.className = "arrangement-stage";
  stage.dataset.sharedTransition = "active-arrangement";

  const svg = svgNode("svg");
  svg.classList.add("arrangement-canvas");
  svg.setAttribute("role", "group");
  svg.setAttribute("aria-label", "Display arrangement");
  svg.setAttribute("aria-describedby", "arrangement-instructions");
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  stage.append(svg);

  let legend = legendFor(false);
  function legendFor(withShared) {
    return createLegend([
      ...members.order.map((key) => ({
        kind: "group",
        label: labels[key],
        side: key,
        tone: tones[key],
      })),
      { kind: "primary", label: "Primary display" },
      ...(withShared ? [{ kind: "shared", label: "Cabled to both computers" }] : []),
      { kind: "seam", label: "Pointer crossing" },
    ]);
  }

  // One row per monitor cabled to more than one computer: which computer shows on it decides which one draws it.
  const sharedRow = document.createElement("div");
  sharedRow.className = "arrangement-shared";
  const sharedPanel = revealPanel(sharedRow);

  // One switch per display each computer reports; off leaves it out of the picture and every route.
  const useBlock = document.createElement("div");
  useBlock.className = "arrangement-use";
  const usePanel = revealPanel(useBlock);

  const controls = document.createElement("div");
  controls.className = "arrangement-controls";
  controls.setAttribute("aria-label", "Arrangement actions");
  const spacer = document.createElement("span");
  spacer.className = "arrangement-controls-spacer";
  const reset = controlButton(
    "Reset",
    options.resetHint ?? "Restore the last applied arrangement",
    disabled || options.canReset !== true,
  );
  reset.dataset.arrangementReset = "true";
  reset.addEventListener("click", () => {
    refitNext = true;
    handlers.onReset?.();
  });
  const fit = controlButton("Fit", "Fit every computer in this canvas", false);
  fit.dataset.arrangementFit = "true";
  fit.addEventListener("click", () => {
    refitNext = true;
    renderScene();
  });
  controls.append(spacer, reset, fit);

  root.append(heading, instructions, stage, legend, sharedPanel, usePanel, controls);

  if (!arrangement?.groups?.order?.length || !arrangement.placement) {
    status.textContent = arrangement?.message || "These displays cannot be arranged yet.";
    stage.dataset.state = "empty";
    const empty = document.createElement("p");
    empty.className = "arrangement-empty";
    empty.textContent =
      "No displays to arrange. Reconnect every computer, then come back to this step.";
    stage.append(empty);
    for (const control of controls.querySelectorAll("button")) control.disabled = true;
    return { element: root, update() {}, destroy() {} };
  }

  let groups = arrangement.groups;
  let pending = null;
  let destroyed = false;
  let frame = null;
  let drag = null;
  let transform = null;
  let stageSize = { width: MIN_STAGE_WIDTH, height: MIN_STAGE_HEIGHT };
  let rects = { tiles: {}, groups: {} };
  let nodes = {};
  let rendered = { shape: null, scale: null, disabled: null };
  let groupLayer = null;
  let seamLayer = null;
  let previewLayer = null;
  let guideLayer = null;
  // A block that leaves the group keeps fading here after `nodes`/`groupLayer` no longer hold it;
  // it removes itself once its leave animation ends, so this layer persists across rebuilds.
  const leavingLayer = svgNode("g");
  leavingLayer.classList.add("arrangement-leaving");
  leavingLayer.setAttribute("aria-hidden", "true");
  let announced = "";

  const resizeObserver =
    typeof ResizeObserver === "function" ? new ResizeObserver(() => scheduleRender()) : null;
  resizeObserver?.observe(stage);

  function announce(message) {
    if (message === announced) return;
    announced = message;
    status.textContent = message;
  }

  function restingStatus() {
    announce(
      arrangement.connected
        ? describeArrangement(arrangement)
        : arrangement.message || "These computers are not connected yet.",
    );
  }

  function stageMetrics() {
    const box = stage.getBoundingClientRect();
    return {
      width: Math.max(MIN_STAGE_WIDTH, Math.round(box.width || MIN_STAGE_WIDTH)),
      height: Math.max(MIN_STAGE_HEIGHT, Math.round(box.height || MIN_STAGE_HEIGHT)),
    };
  }

  function renderShared() {
    const withShared = arrangement.tiles.some((tile) => tile.shared);
    if (Boolean(legend.dataset.shared) !== withShared) {
      const next = legendFor(withShared);
      if (withShared) next.dataset.shared = "true";
      legend.replaceWith(next);
      legend = next;
    }
    // A block on its way out keeps what it last showed while it glides shut.
    if (shared.length) sharedRow.replaceChildren();
    for (const choice of shared) {
      const copy = document.createElement("span");
      copy.className = "arrangement-shared-copy";
      const name = document.createElement("strong");
      name.textContent = choice.name;
      copy.append(name, document.createTextNode(" is cabled to both computers. Shows"));
      const control = document.createElement("div");
      control.className = "segmented is-compact";
      control.setAttribute("role", "radiogroup");
      control.setAttribute("aria-label", `Which computer shows on ${choice.name}`);
      for (const key of groups.order) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "segmented-option";
        button.dataset.focusKey = `arrangement-shared-${choice.monitor}-${key}`;
        button.dataset.sharedMonitor = choice.monitor;
        button.dataset.sharedSide = key;
        button.setAttribute("role", "radio");
        button.setAttribute("aria-checked", String(choice.side === key));
        button.textContent = labels[key];
        const locked = choice.side !== key && !choice.canSwap;
        button.disabled = disabled || locked;
        button.title = locked
          ? `${labels[key]} would keep no display of its own.`
          : `${choice.name} shows ${labels[key]}, so the pointer crosses onto it as that computer.`;
        button.addEventListener("click", () => {
          if (disabled || choice.side === key) return;
          refitNext = true;
          handlers.onShowMonitor?.(choice.monitor, key);
        });
        control.append(button);
      }
      const item = document.createElement("div");
      item.className = "arrangement-shared-item";
      item.append(copy, control);
      sharedRow.append(item);
    }
    setRevealOpen(sharedPanel, shared.length > 0, { focusTarget: fit });
  }

  function renderUse() {
    const withDisplays = groups.order.filter((key) => inUse[key]?.length);
    if (!withDisplays.length) {
      setRevealOpen(usePanel, false, { focusTarget: fit });
      return;
    }
    const useHeading = document.createElement("div");
    useHeading.className = "arrangement-use-heading";
    const useTitle = document.createElement("span");
    useTitle.textContent = "Displays in use";
    const useHint = document.createElement("span");
    useHint.className = "arrangement-use-hint";
    useHint.textContent = "Turn off a display that is showing another computer or is not in use.";
    useHeading.append(useTitle, useHint);
    const columns = document.createElement("div");
    columns.className = "arrangement-use-groups";
    for (const key of withDisplays) {
      const group = document.createElement("div");
      group.className = "arrangement-use-group";
      group.dataset.side = key;
      const name = document.createElement("div");
      name.className = "arrangement-legend-item arrangement-use-title";
      name.dataset.side = key;
      name.dataset.tone = tones[key];
      const swatch = document.createElement("span");
      swatch.className = "arrangement-legend-swatch";
      swatch.setAttribute("aria-hidden", "true");
      const text = document.createElement("span");
      text.textContent = labels[key];
      name.append(swatch, text);
      group.append(name);
      for (const display of inUse[key]) {
        const locked = display.inUse && !display.canLeave;
        const details = [formatSize(display.size[0], display.size[1])];
        if (display.primary) details.push("Primary");
        if (display.cabledToBoth) details.push("Cabled to both computers");
        if (locked) details.push("Its computer's only display");
        const row = switchRow(display.name, {
          checked: display.inUse,
          description: details.join(" · "),
          disabled: disabled || locked,
          focusKey: `arrangement-use-${display.id}`,
          onChange: (next) => {
            if (disabled || locked) return;
            refitNext = true;
            handlers.onUseDisplay?.(display.id, next);
          },
        });
        row.dataset.useDisplay = display.id;
        group.append(row);
      }
      columns.append(group);
    }
    useBlock.replaceChildren(useHeading, columns);
    setRevealOpen(usePanel, true);
  }

  function renderScene() {
    if (destroyed || drag) return;
    if (frame !== null) cancelAnimationFrame(frame);
    frame = null;
    if (pending) {
      arrangement = pending;
      groups = arrangement.groups;
      pending = null;
    }
    const focused = svg.contains(document.activeElement)
      ? focusKeyOf(document.activeElement)
      : null;
    // Measuring the stage also flushes the previous positions, so a moved group animates to its new one.
    stageSize = stageMetrics();
    svg.setAttribute("viewBox", `0 0 ${stageSize.width} ${stageSize.height}`);
    renderShared();
    renderUse();

    const all = arrangement.tiles;
    // Nothing to place means nothing to measure: the message alone is the scene until displays return.
    if (!all.length) {
      svg.replaceChildren();
      nodes = {};
      rendered = { shape: null, scale: null, disabled: null };
      stage.dataset.state = "empty";
      stage.dataset.dragging = "false";
      announce(arrangement.message || "These displays cannot be arranged yet.");
      return;
    }
    const shape = shapeOf(arrangement);
    const key = `${shape}@${stageSize.width}x${stageSize.height}`;
    const ideal = fitTransform(all, stageSize);
    const stored = storedView?.key === key && !refitNext ? storedView.transform : null;
    // Edits pan the kept view instead of rescaling it; only a layout too large for that scale is refitted.
    transform = constrainTransform(all, stored, stageSize) ?? ideal;
    refitNext = false;
    storedView = { key, transform };
    fit.classList.toggle("is-current", sameTransform(transform, ideal));

    const previousGroupRects = rects.groups;
    const previousNodes = nodes;
    rects = { tiles: tileRects(all, transform), groups: groupRects(all, transform) };
    const boxes = labelBoxes(rects.groups, labels, stageSize, VIEW_INSETS);
    stage.dataset.state = arrangement.connected
      ? "connected"
      : arrangement.valid
        ? "loose"
        : "invalid";
    stage.dataset.dragging = "false";

    // The same displays at the same places and scale only need fresh seams; anything else is rebuilt.
    const sameSet =
      rendered.shape !== null &&
      rendered.shape === shape &&
      rendered.scale === transform.scale &&
      rendered.disabled === disabled;
    if (sameSet) {
      for (const groupKey of groups.order) refreshGroup(groupKey, boxes[groupKey]);
      replaceLayer("guideLayer", createGuideLayer([], stageSize));
      replaceLayer(
        "seamLayer",
        createSeamLayer(arrangement.connected ? arrangement.seams : [], transform),
      );
      replaceLayer("previewLayer", emptyPreview());
    } else {
      guideLayer = createGuideLayer([], stageSize);
      groupLayer = svgNode("g");
      groupLayer.classList.add("arrangement-groups");
      const nextNodes = {};
      for (const groupKey of groups.order)
        nextNodes[groupKey] = buildGroup(groupKey, boxes[groupKey]);
      for (const groupKey of groups.order) groupLayer.append(nextNodes[groupKey]);
      nodes = nextNodes;
      seamLayer = createSeamLayer(arrangement.connected ? arrangement.seams : [], transform);
      previewLayer = emptyPreview();
      svg.replaceChildren(guideLayer, groupLayer, leavingLayer, seamLayer, previewLayer);
      playGroupMotion(groupMotion(previousGroupRects, rects.groups), previousNodes);
    }
    rendered = { shape, scale: transform.scale, disabled };
    refineText(svg);
    restingStatus();
    if (!disabled && focused)
      svg.querySelector(`[data-focus-key="${focused}"]`)?.focus({ preventScroll: true });
  }

  // Plays a full-rebuild's motion: continuing groups FLIP from their last rect, a joining group
  // fades and scales in (`data-motion="enter"`, styled in styles.css), and a group that just left
  // keeps its last node fading in `leavingLayer` until its own leave animation ends. Reduced motion
  // skips all of it: the rebuild already drew every node at its true, final position.
  function playGroupMotion(motion, previousNodes) {
    if (reducedMotion()) return;
    const settled = [];
    for (const [groupKey, [dx, dy]] of Object.entries(motion.moved)) {
      const node = nodes[groupKey];
      const rect = rects.groups[groupKey];
      if (!node || !rect) continue;
      setPosition(node, rect.x + dx, rect.y + dy);
      settled.push({ node, rect });
    }
    for (const groupKey of motion.entered) {
      const node = nodes[groupKey];
      if (node) node.dataset.motion = "enter";
    }
    if (settled.length)
      requestAnimationFrame(() => {
        // Reading the box lays the offset out, so setting the true position below is a change the transition can run.
        for (const { node } of settled) node.getBoundingClientRect();
        for (const { node, rect } of settled) setPosition(node, rect.x, rect.y);
      });
    for (const groupKey of motion.left) {
      const node = previousNodes?.[groupKey];
      if (!node) continue;
      node.dataset.motion = "leave";
      node.removeAttribute("tabindex");
      node.style.pointerEvents = "none";
      leavingLayer.append(node);
      node.addEventListener("animationend", () => node.remove(), { once: true });
    }
  }

  function replaceLayer(name, next) {
    const layers = { guideLayer, seamLayer, previewLayer };
    svg.replaceChild(next, layers[name]);
    if (name === "guideLayer") guideLayer = next;
    else if (name === "seamLayer") seamLayer = next;
    else previewLayer = next;
  }

  function buildGroup(groupKey, labelBox) {
    const node = createGroupNode({
      tiles: arrangement.tiles.filter((t) => (t.group ?? t.side) === groupKey),
      tileRects: rects.tiles,
      groupKey,
      platform: platforms[groupKey],
      side: groupKey,
      tone: tones[groupKey],
      label: labels[groupKey],
      rect: rects.groups[groupKey],
      labelBox,
      focusable: !disabled,
      focusTiles: false,
      ariaLabel: groupAria(groupKey),
      tileAria: (tile) => tileAria(tile, groupKey),
    });
    if (!disabled) {
      node.addEventListener("pointerdown", (event) => beginDrag(event, groupKey));
      node.addEventListener("keydown", (event) => onKeyDown(event, groupKey));
      node.addEventListener("focusout", (event) => {
        if (drag?.keyboard && !node.contains(event.relatedTarget)) cancelDrag();
      });
    }
    return node;
  }

  // Same displays at the same scale: move the tiles that exist instead of drawing them again.
  function refreshGroup(groupKey, labelBox) {
    const node = nodes[groupKey];
    node.classList.remove("is-dragging");
    node.setAttribute("aria-label", groupAria(groupKey));
    node.replaceChild(
      createGroupLabelNode(labels[groupKey], labelBox, rects.groups[groupKey]),
      node.firstChild,
    );
    setPosition(node, rects.groups[groupKey].x, rects.groups[groupKey].y);
  }

  function groupAria(groupKey) {
    return groupAriaText({
      key: groupKey,
      tiles: arrangement.tiles,
      groups,
      rects: rects.groups,
      labels,
      connected: arrangement.connected,
      crossingText: arrangement.connected ? describeArrangement(arrangement) : "",
    });
  }

  function tileAria(tile, groupKey) {
    return tileAriaText({
      tile,
      groupLabel: labels[groupKey],
      seams: arrangement.seams,
      otherGroupCount: groups.order.length - 1,
    });
  }

  function scheduleRender() {
    if (destroyed || drag || frame !== null) return;
    frame = requestAnimationFrame(() => renderScene());
  }

  function candidateFor(deltaX, deltaY) {
    const moved = movePlacement(groups, arrangement.placement, drag.moving, [
      deltaX / transform.scale,
      deltaY / transform.scale,
    ]);
    return resolvePlacement(
      groups,
      snapPlacement(groups, moved, drag.moving, SNAP_PIXELS / transform.scale),
      drag.moving,
    );
  }

  function drawPreview(candidate) {
    const layer = emptyPreview();
    const guides = [];
    const geometry = candidate ? arrangementGeometry(groups, candidate) : null;
    if (candidate) {
      const ids = new Set(movingIds(groups, drag.moving));
      const outline = tileRects(
        tiles(groups, candidate).filter((t) => ids.has(t.id)),
        transform,
      );
      layer.append(createOutlineNode(Object.values(outline), "arrangement-drop-outline"));
      if (geometry.connected) {
        layer.append(createSeamLayer(geometry.seams, transform, { preview: true }));
        for (const seam of geometry.seams) {
          const vertical =
            Math.abs(seam.start[0] - seam.end[0]) < Math.abs(seam.start[1] - seam.end[1]);
          guides.push(
            vertical
              ? { axis: "x", at: transform.originX + seam.start[0] * transform.scale }
              : { axis: "y", at: transform.originY + seam.start[1] * transform.scale },
          );
        }
      }
    }
    replaceLayer("guideLayer", createGuideLayer(dedupeGuides(guides), stageSize));
    replaceLayer("previewLayer", layer);
    stage.dataset.drop = candidate ? "valid" : "none";
    if (!candidate) announce("No touching position here. Release to keep the current arrangement.");
    else announce(`Drop here. ${describeArrangement(geometry)}`);
  }

  function moveDrag(deltaX, deltaY) {
    drag.deltaX = deltaX;
    drag.deltaY = deltaY;
    drag.moved ||= Math.abs(deltaX) > 1 || Math.abs(deltaY) > 1;
    setPosition(drag.node, drag.baseX + deltaX, drag.baseY + deltaY);
    drag.resolved = candidateFor(deltaX, deltaY);
    drawPreview(drag.resolved);
  }

  function startDrag(moving, keyboard) {
    const node = nodes[moving.group];
    // Raising the group above the others moves it in the DOM, which drops focus: take both before the drag exists.
    groupLayer.append(node);
    node?.focus({ preventScroll: true });
    const base = rects.groups[moving.group];
    drag = {
      moving,
      node,
      keyboard,
      pointerId: null,
      startClientX: 0,
      startClientY: 0,
      baseX: base.x,
      baseY: base.y,
      deltaX: 0,
      deltaY: 0,
      moved: false,
      resolved: arrangement.placement,
    };
    stage.dataset.dragging = "true";
    stage.dataset.drop = "valid";
    seamLayer.setAttribute("visibility", "hidden");
    node?.classList.add("is-dragging");
    return node;
  }

  function endDrag() {
    if (!drag) return null;
    const current = drag;
    drag = null;
    stage.dataset.dragging = "false";
    delete stage.dataset.drop;
    seamLayer.removeAttribute("visibility");
    current.node?.classList.remove("is-dragging");
    if (current.pointerId !== null) {
      listenForPointer(false);
      try {
        svg.releasePointerCapture(current.pointerId);
      } catch {
        // Capture is already gone after a cancelled pointer.
      }
    }
    return current;
  }

  // The app re-mounts this editor on every status poll, which drops pointer capture; the window
  // still sees every move and release, and capture is taken again once the editor is back.
  function listenForPointer(active) {
    for (const [type, handler] of pointerListeners) {
      if (active) window.addEventListener(type, handler);
      else window.removeEventListener(type, handler);
    }
  }

  function capturePointer() {
    if (!drag || drag.keyboard || drag.pointerId === null || !svg.isConnected) return;
    try {
      svg.setPointerCapture(drag.pointerId);
    } catch {
      // A pointer that is no longer pressed cannot be captured; its release already arrived.
    }
  }

  function cancelDrag() {
    if (!endDrag()) return;
    renderScene();
    announce("Move cancelled. The arrangement is unchanged.");
  }

  function commitDrag(current) {
    if (!current) return;
    if (!current.moved || !current.resolved) {
      renderScene();
      if (current.moved) announce("No touching position there, so the arrangement is unchanged.");
      return;
    }
    refitNext = true;
    handlers.onCommit?.(current.resolved, current.moving);
  }

  const beginDrag = (event, groupKey) => {
    if (destroyed || disabled || event.button !== 0 || !transform) return;
    event.preventDefault();
    if (drag) endDrag();
    startDrag({ group: groupKey }, false);
    drag.pointerId = event.pointerId;
    drag.startClientX = event.clientX;
    drag.startClientY = event.clientY;
    listenForPointer(true);
    capturePointer();
  };

  const onPointerMove = (event) => {
    if (destroyed || !drag || drag.keyboard || event.pointerId !== drag.pointerId) return;
    moveDrag(
      clampDelta(event.clientX - drag.startClientX, stageSize.width),
      clampDelta(event.clientY - drag.startClientY, stageSize.height),
    );
  };

  const finishDrag = (event) => {
    if (destroyed || !drag || drag.keyboard || event.pointerId !== drag.pointerId) return;
    moveDrag(
      clampDelta(event.clientX - drag.startClientX, stageSize.width),
      clampDelta(event.clientY - drag.startClientY, stageSize.height),
    );
    commitDrag(endDrag());
  };

  const onPointerCancel = (event) => {
    if (
      !drag ||
      drag.keyboard ||
      (event?.pointerId !== undefined && event.pointerId !== drag.pointerId)
    )
      return;
    cancelDrag();
  };

  function onKeyDown(event, groupKey) {
    if (destroyed || disabled || !transform) return;
    const moving = { group: groupKey };
    const same = drag?.keyboard && drag.moving.group === moving.group;
    if (event.key === "Escape") {
      if (!drag) return;
      event.preventDefault();
      cancelDrag();
      return;
    }
    if (event.key === "Enter" || event.key === " " || event.key === "Spacebar") {
      event.preventDefault();
      if (same) {
        commitDrag(endDrag());
        return;
      }
      if (drag) endDrag();
      startDrag(moving, true);
      announce(
        `${labels[groupKey]} picked up. Arrow keys move it, Enter drops it, Escape cancels.`,
      );
      return;
    }
    const pixels = nudgePixels(event.key, event.shiftKey);
    if (!pixels) return;
    event.preventDefault();
    if (same) {
      moveDrag(drag.deltaX + pixels[0], drag.deltaY + pixels[1]);
      return;
    }
    const target = nudgeTarget(
      groups,
      arrangement.placement,
      moving,
      event.key,
      event.shiftKey,
      transform.scale,
    );
    if (!target) {
      announce("That direction has no touching position.");
      return;
    }
    handlers.onCommit?.(target, moving);
  }

  const onStageKeyDown = (event) => {
    if (event.key === "Escape" && drag) cancelDrag();
  };

  const pointerListeners = [
    ["pointermove", onPointerMove],
    ["pointerup", finishDrag],
    ["pointercancel", onPointerCancel],
  ];
  stage.addEventListener("keydown", onStageKeyDown);
  renderScene();

  return {
    element: root,
    // The app re-renders on every status poll; a live gesture must survive that, so state arrives here instead.
    update(next = {}) {
      if (destroyed) return;
      if (next.handlers) handlers = { ...handlers, ...next.handlers };
      if (Array.isArray(next.shared)) shared = next.shared;
      if (Array.isArray(next.members)) {
        const nextMembers = membersFromOptions({ members: next.members });
        if (nextMembers) {
          members = nextMembers;
          ({ labels, platforms, tones } = memberMaps(members));
        }
      }
      if (next.inUse) inUse = normalizeUseChoices(groups?.order ?? members.order, next.inUse);
      if (typeof next.disabled === "boolean") disabled = next.disabled;
      reset.disabled = disabled || next.canReset !== true;
      if (next.resetHint) reset.title = next.resetHint;
      // A gesture cannot outlive the editing it belongs to, so going busy drops it.
      if (disabled && drag) endDrag();
      const incoming = next.arrangement;
      if (incoming?.groups?.order?.length && incoming.placement) {
        if (drag && shapeOf(incoming) === shapeOf(arrangement)) {
          pending = incoming;
          capturePointer();
          return;
        }
        if (drag) endDrag();
        arrangement = incoming;
        groups = arrangement.groups;
      }
      renderScene();
    },
    destroy() {
      if (destroyed) return;
      destroyed = true;
      endDrag();
      resizeObserver?.disconnect();
      if (frame !== null) cancelAnimationFrame(frame);
      listenForPointer(false);
      stage.removeEventListener("keydown", onStageKeyDown);
    },
  };
}

function dedupeGuides(guides) {
  const seen = new Set();
  return guides.filter((guide) => {
    const key = `${guide.axis}:${Math.round(guide.at)}`;
    if (seen.has(key) || !Number.isFinite(guide.at)) return false;
    seen.add(key);
    return true;
  });
}

function clampDelta(value, extent) {
  return Math.min(Math.max(value, -extent * 4), extent * 4);
}

function controlButton(label, title, disabled) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "arrangement-control";
  button.textContent = label;
  button.title = title;
  button.disabled = disabled;
  return button;
}
