import {
  VIEW_INSETS,
  arrangementGeometry,
  constrainTransform,
  describeArrangement,
  fitTransform,
  formatSize,
  movePlacement,
  movingIds,
  resolvePlacement,
  sameTransform,
  sideRects,
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
import { switchRow } from "./dom.mjs";

const MIN_STAGE_WIDTH = 280;
const MIN_STAGE_HEIGHT = 190;
const SNAP_PIXELS = 14;
const NUDGE_PIXELS = 12;
const COARSE_NUDGE_PIXELS = 48;
const SIDES = ["local", "peer"];
const INSTRUCTIONS =
  "Drag a computer against the other; the edge where they touch is where the pointer crosses. With a computer selected, arrow keys nudge it and Shift with an arrow moves it further.";

// Only one editor is mounted at a time, so the fitted view survives the rebuild a commit triggers.
let storedView = null;
let refitNext = true;

function shapeOf(value) {
  return `${value.tiles.map((t) => `${t.id}:${t.side}:${t.x}:${t.y}:${t.width}:${t.height}`).join(",")}`;
}

function focusKeyOf(node) {
  return node?.dataset?.focusKey ?? null;
}

function emptyPreview() {
  const layer = svgNode("g");
  layer.classList.add("arrangement-preview");
  return layer;
}

export function createArrangementView(options) {
  const {
    localPlatform,
    peerPlatform,
    localLabel: localOverride,
    peerLabel: peerOverride,
  } = options;
  let arrangement = options.arrangement;
  let shared = Array.isArray(options.shared) ? options.shared : [];
  let inUse = useChoices(options.inUse);
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

  const localName = localOverride ?? "This computer";
  const peerName = peerOverride ?? "The other computer";
  const labels = { local: localName, peer: peerName };
  const platforms = { local: localPlatform, peer: peerPlatform };

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
      { kind: "group", label: localName, side: "local" },
      { kind: "group", label: peerName, side: "peer" },
      { kind: "primary", label: "Primary display" },
      ...(withShared ? [{ kind: "shared", label: "Cabled to both computers" }] : []),
      { kind: "seam", label: "Pointer crossing" },
    ]);
  }

  // One row per monitor cabled to both computers: which computer shows on it decides which side draws it.
  const sharedRow = document.createElement("div");
  sharedRow.className = "arrangement-shared";
  sharedRow.hidden = true;

  // One switch per display each computer reports; off leaves it out of the picture and every route.
  const useBlock = document.createElement("div");
  useBlock.className = "arrangement-use";
  useBlock.hidden = true;

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
  const fit = controlButton("Fit", "Fit both computers in this canvas", false);
  fit.dataset.arrangementFit = "true";
  fit.addEventListener("click", () => {
    refitNext = true;
    renderScene();
  });
  controls.append(spacer, reset, fit);

  root.append(heading, instructions, stage, legend, sharedRow, useBlock, controls);

  if (!arrangement?.groups?.local || !arrangement.groups?.peer || !arrangement.placement) {
    status.textContent = arrangement?.message || "These displays cannot be arranged yet.";
    stage.dataset.state = "empty";
    const empty = document.createElement("p");
    empty.className = "arrangement-empty";
    empty.textContent =
      "No displays to arrange. Reconnect both computers, then come back to this step.";
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
  let rects = { tiles: {}, sides: {} };
  let nodes = { local: null, peer: null };
  let rendered = { shape: null, scale: null, disabled: null };
  let groupLayer = null;
  let seamLayer = null;
  let previewLayer = null;
  let guideLayer = null;
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
    sharedRow.replaceChildren();
    sharedRow.hidden = !shared.length;
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
      for (const side of SIDES) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "segmented-option";
        button.dataset.focusKey = `arrangement-shared-${choice.monitor}-${side}`;
        button.dataset.sharedMonitor = choice.monitor;
        button.dataset.sharedSide = side;
        button.setAttribute("role", "radio");
        button.setAttribute("aria-checked", String(choice.side === side));
        button.textContent = labels[side];
        const locked = choice.side !== side && !choice.canSwap;
        button.disabled = disabled || locked;
        button.title = locked
          ? `${labels[side]} would keep no display of its own.`
          : `${choice.name} shows ${labels[side]}, so the pointer crosses onto it as that computer.`;
        button.addEventListener("click", () => {
          if (disabled || choice.side === side) return;
          refitNext = true;
          handlers.onShowMonitor?.(choice.monitor, side);
        });
        control.append(button);
      }
      const item = document.createElement("div");
      item.className = "arrangement-shared-item";
      item.append(copy, control);
      sharedRow.append(item);
    }
  }

  function renderUse() {
    useBlock.replaceChildren();
    const withDisplays = SIDES.filter((side) => inUse[side].length);
    useBlock.hidden = !withDisplays.length;
    if (useBlock.hidden) return;
    const useHeading = document.createElement("div");
    useHeading.className = "arrangement-use-heading";
    const useTitle = document.createElement("span");
    useTitle.textContent = "Displays in use";
    const useHint = document.createElement("span");
    useHint.className = "arrangement-use-hint";
    useHint.textContent = "Turn off a display that is showing the other computer or is not in use.";
    useHeading.append(useTitle, useHint);
    const columns = document.createElement("div");
    columns.className = "arrangement-use-groups";
    for (const side of withDisplays) {
      const group = document.createElement("div");
      group.className = "arrangement-use-group";
      group.dataset.side = side;
      const name = document.createElement("div");
      name.className = "arrangement-legend-item arrangement-use-title";
      name.dataset.side = side;
      const swatch = document.createElement("span");
      swatch.className = "arrangement-legend-swatch";
      swatch.setAttribute("aria-hidden", "true");
      const text = document.createElement("span");
      text.textContent = labels[side];
      name.append(swatch, text);
      group.append(name);
      for (const display of inUse[side]) {
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
    useBlock.append(useHeading, columns);
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
      nodes = { local: null, peer: null };
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

    rects = { tiles: tileRects(all, transform), sides: sideRects(all, transform) };
    const boxes = labelBoxes(rects.sides, labels, stageSize, VIEW_INSETS);
    stage.dataset.state = arrangement.connected
      ? "connected"
      : arrangement.valid
        ? "loose"
        : "invalid";
    stage.dataset.dragging = "false";

    // The same displays at the same places and scale only need fresh seams; anything else is rebuilt.
    if (
      nodes.local &&
      rendered.shape === shape &&
      rendered.scale === transform.scale &&
      rendered.disabled === disabled
    ) {
      for (const groupKey of SIDES) refreshGroup(groupKey, boxes[groupKey]);
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
      nodes = {
        local: buildGroup("local", boxes.local),
        peer: buildGroup("peer", boxes.peer),
      };
      groupLayer.append(nodes.local, nodes.peer);
      seamLayer = createSeamLayer(arrangement.connected ? arrangement.seams : [], transform);
      previewLayer = emptyPreview();
      svg.replaceChildren(guideLayer, groupLayer, seamLayer, previewLayer);
    }
    rendered = { shape, scale: transform.scale, disabled };
    refineText(svg);
    restingStatus();
    if (!disabled && focused)
      svg.querySelector(`[data-focus-key="${focused}"]`)?.focus({ preventScroll: true });
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
      tiles: arrangement.tiles.filter((t) => t.side === groupKey),
      tileRects: rects.tiles,
      groupKey,
      platform: platforms[groupKey],
      side: groupKey,
      label: labels[groupKey],
      rect: rects.sides[groupKey],
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
      createGroupLabelNode(labels[groupKey], labelBox, rects.sides[groupKey]),
      node.firstChild,
    );
    setPosition(node, rects.sides[groupKey].x, rects.sides[groupKey].y);
  }

  function groupAria(groupKey) {
    const own = arrangement.tiles.filter((t) => t.side === groupKey);
    const count = own.length;
    const other = groupKey === "local" ? "peer" : "local";
    const place = `Sits ${sideWord(groupKey)} of ${labels[other]}.`;
    const crossing = arrangement.connected ? describeArrangement(arrangement) : "Not touching yet.";
    const size = formatSize(groups[groupKey].width, groups[groupKey].height);
    return `${labels[groupKey]}. ${count} display${count === 1 ? "" : "s"}, ${size} together. ${place} ${crossing} Drag, or use the arrow keys.`;
  }

  function tileAria(tile, groupKey) {
    const seams = arrangement.seams.filter(
      (s) => (groupKey === "local" ? s.fromDisplay : s.toDisplay) === tile.id,
    );
    const contact = seams.length
      ? `Crosses on its ${seams.map((s) => (groupKey === "local" ? s.fromEdge : s.toEdge)).join(" and ")} edge.`
      : "Not touching the other computer.";
    return `${tile.name}, ${formatSize(tile.width, tile.height)}${tile.primary ? ", primary display" : ""}, on ${labels[groupKey]}. ${contact} Drag, or use the arrow keys.`;
  }

  function sideWord(groupKey) {
    const local = rects.sides.local;
    const peer = rects.sides.peer;
    const word =
      peer.x >= local.x + local.width
        ? "to the right"
        : peer.x + peer.width <= local.x
          ? "to the left"
          : peer.y >= local.y + local.height
            ? "below"
            : peer.y + peer.height <= local.y
              ? "above"
              : "beside";
    if (groupKey === "peer") return word;
    return {
      "to the right": "to the left",
      "to the left": "to the right",
      below: "above",
      above: "below",
      beside: "beside",
    }[word];
  }

  function scheduleRender() {
    if (destroyed || drag || frame !== null) return;
    frame = requestAnimationFrame(() => renderScene());
  }

  function movingNode(moving) {
    return nodes[moving.side];
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
    const node = movingNode(moving);
    // Raising the group above the other one moves it in the DOM, which drops focus: take both before the drag exists.
    groupLayer.append(nodes[moving.side]);
    node?.focus({ preventScroll: true });
    const base = rects.sides[moving.side];
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
    startDrag({ side: groupKey }, false);
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
    const moving = { side: groupKey };
    const same = drag?.keyboard && drag.moving.side === moving.side;
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
    const direction = {
      ArrowLeft: [-1, 0],
      ArrowRight: [1, 0],
      ArrowUp: [0, -1],
      ArrowDown: [0, 1],
    }[event.key];
    if (!direction) return;
    event.preventDefault();
    const step = event.shiftKey ? COARSE_NUDGE_PIXELS : NUDGE_PIXELS;
    if (same) {
      moveDrag(drag.deltaX + direction[0] * step, drag.deltaY + direction[1] * step);
      return;
    }
    const nudged = movePlacement(groups, arrangement.placement, moving, [
      (direction[0] * step) / transform.scale,
      (direction[1] * step) / transform.scale,
    ]);
    const target = resolvePlacement(groups, nudged, moving);
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
      if (next.inUse) inUse = useChoices(next.inUse);
      if (typeof next.disabled === "boolean") disabled = next.disabled;
      reset.disabled = disabled || next.canReset !== true;
      if (next.resetHint) reset.title = next.resetHint;
      // A gesture cannot outlive the editing it belongs to, so going busy drops it.
      if (disabled && drag) endDrag();
      const incoming = next.arrangement;
      if (incoming?.groups?.local && incoming.groups?.peer && incoming.placement) {
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

function useChoices(value) {
  return {
    local: Array.isArray(value?.local) ? value.local : [],
    peer: Array.isArray(value?.peer) ? value.peer : [],
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
