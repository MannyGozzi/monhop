// The floating menus over the arranging canvas: the saved arrangements, and the displays each
// computer uses, which a click on a display also opens. A menu, its pages and their rows outlive
// renders, so a menu glides between anchors and resizes in place, a row keeps its hover and focus,
// and a switch mid-flip carries on; each render only hands them new state.
import { svgNode } from "./arrangement-render.mjs";
import { MENU_GAP, MENU_INSET, menuPlacement, thumbnailRects } from "./canvas-menus-model.mjs";
import { layoutChipStrip } from "./computer-card.mjs";
import {
  button,
  el,
  ghostOf,
  handOffFocus,
  motionEase,
  motionEnabled,
  motionMs,
  stableLabel,
} from "./dom.mjs";
import { MAX_ARRANGEMENT_NAME, normalizeArrangementName } from "./sharing-model.mjs";

// A saved arrangement's drawing in its own units; the stylesheet sets the size it shows at.
const THUMB_BOX = Object.freeze({ width: 44, height: 30 });
const THUMB_INSET = 0.75;
// Rows on their way out, which keep their place until they have closed.
const leavingRows = new WeakSet();
// Rows opening or closing; the menu sized by them follows them, so it never glides over them.
const movingRows = new Set();

// --- the menu ----------------------------------------------------------------

// One menu shell for the pane. `show` points it at an anchor and writes a page into it; a menu
// already open glides to the new anchor, and cross-fades when it shows something else now.
// Escape, or a press outside it and its openers, asks `onDismiss` to close it.
export function createCanvasMenu(pane, { onDismiss }) {
  const body = el("div", { className: "canvas-menu-body" });
  const surface = el("div", {
    className: "canvas-menu-surface",
    attrs: { role: "dialog" },
    children: [body],
  });
  const root = el("div", {
    className: "canvas-menu",
    dataset: { open: "false", side: "below", glide: "true" },
    children: [surface],
  });
  root.inert = true;
  pane.append(root);

  let current = null;
  let open = false;
  let anchorBox = null;
  let sizeGlide = null;
  // Counts what the menu has shown, so a press outside that opened something else leaves that open.
  let shownCount = 0;

  // Rows that open or close resize the body as they go, and the surface, sized by it, follows them;
  // only the anchor point has to be worked out again.
  const sizeWatch = new ResizeObserver(() => place());
  sizeWatch.observe(body);
  sizeWatch.observe(pane);

  function show(next) {
    const opening = !open;
    const from = opening ? null : drawnSize();
    if (opening || next.page !== current?.page || next.swap) shownCount += 1;
    const write = () => {
      if (body.firstElementChild !== next.page.element) body.replaceChildren(next.page.element);
      next.update();
    };
    if (!opening && (next.page !== current?.page || next.swap)) crossFade(write);
    else write();
    current = next;
    root.dataset.kind = next.kind;
    surface.setAttribute("aria-label", next.label);
    if (!opening) {
      glideSize(from);
      place();
      return;
    }
    open = true;
    root.inert = false;
    // A menu still fading out glides to its new place; a hidden one appears there.
    place({ jump: !shown() });
    root.dataset.open = "true";
    listen(true);
  }

  function hide() {
    if (!open) return;
    open = false;
    handOffFocus(root, current?.returnFocus?.() ?? null);
    root.dataset.open = "false";
    root.inert = true;
    listen(false);
    current?.page.close?.();
  }

  // The anchor is read again on every call, so a display that moved takes the menu with it; one
  // that left the picture keeps the menu where it last pointed.
  function place({ jump = false } = {}) {
    if (!open || !current) return;
    const paneBox = pane.getBoundingClientRect();
    const rect = current.anchor();
    if (rect && (rect.width || rect.height))
      anchorBox = {
        left: rect.left - paneBox.left,
        top: rect.top - paneBox.top,
        right: rect.right - paneBox.left,
        bottom: rect.bottom - paneBox.top,
        width: rect.width,
        height: rect.height,
      };
    if (!anchorBox) return;
    const bounds = { left: 0, top: 0, right: paneBox.width, bottom: paneBox.height };
    const room = Math.max(anchorBox.top, bounds.bottom - anchorBox.bottom) - MENU_GAP - MENU_INSET;
    root.style.setProperty("--canvas-menu-room", `${Math.max(Math.floor(room), 0)}px`);
    root.style.setProperty("--canvas-menu-fit", `${Math.max(paneBox.width - MENU_INSET * 2, 0)}px`);
    const box = measure();
    const spot = menuPlacement({
      anchor: anchorBox,
      size: box,
      bounds,
      prefer: current.prefer,
      align: current.align,
    });
    if (jump || !motionEnabled()) root.dataset.glide = "false";
    root.dataset.side = spot.side;
    root.style.setProperty("--canvas-menu-x", `${spot.x}px`);
    root.style.setProperty(
      "--canvas-menu-y",
      `${spot.side === "above" ? spot.y + box.height : spot.y}px`,
    );
    root.style.setProperty("--canvas-menu-origin", `${spot.origin}px`);
    if (root.dataset.glide === "false") {
      root.getBoundingClientRect();
      root.dataset.glide = "true";
    }
  }

  // Content written all at once glides the surface from the size it showed, a glide still running
  // included, before the frame paints.
  function glideSize(from) {
    const next = measure();
    sizeGlide?.cancel();
    if (!motionEnabled() || !shown() || movingRows.size || sameSize(from, next)) return;
    const glide = surface.animate(
      [
        { width: `${from.width}px`, height: `${from.height}px` },
        { width: `${next.width}px`, height: `${next.height}px` },
      ],
      { duration: motionMs("--motion-base"), easing: motionEase("--ease-out") },
    );
    sizeGlide = glide;
    const done = () => {
      if (sizeGlide === glide) sizeGlide = null;
    };
    glide.addEventListener("finish", done);
    glide.addEventListener("cancel", done);
  }

  // What the menu showed fades out over it while the new content rises in.
  function crossFade(write) {
    if (!motionEnabled() || !shown()) {
      write();
      return;
    }
    const ghost = ghostOf(body);
    ghost.classList.add("canvas-menu-ghost");
    ghost.style.width = `${body.offsetWidth}px`;
    surface.append(ghost);
    write();
    const fast = motionMs("--motion-fast");
    const out = ghost.animate([{ opacity: 1 }, { opacity: 0 }], {
      duration: fast,
      easing: motionEase("--ease-in-out"),
      fill: "forwards",
    });
    out.finished.then(
      () => ghost.remove(),
      () => ghost.remove(),
    );
    body.animate([{ opacity: 0 }, { opacity: 1 }], {
      duration: motionMs("--motion-base"),
      delay: fast / 2,
      easing: motionEase("--ease-out"),
      fill: "backwards",
    });
  }

  function measure() {
    return { width: body.offsetWidth, height: body.offsetHeight };
  }

  function drawnSize() {
    const style = getComputedStyle(surface);
    return { width: parseFloat(style.width), height: parseFloat(style.height) };
  }

  function shown() {
    return Number(getComputedStyle(surface).opacity) > 0.01;
  }

  // A press outside closes the menu once its click has landed: the render that closes it rebuilds
  // the Apply bar, and a button replaced under a press never gets the click.
  let pressedOn = null;
  function onPointerDown(event) {
    const outside = !root.contains(event.target) && !event.target.closest?.(current?.openers);
    pressedOn = outside ? shownCount : null;
  }
  function onPointerUp() {
    if (pressedOn === null) return;
    const shownThen = pressedOn;
    pressedOn = null;
    setTimeout(() => {
      if (open && shownCount === shownThen) onDismiss();
    });
  }

  // Taken before the window's own Escape, which would close the inspector sheet under the menu.
  function onKeyDown(event) {
    if (event.key !== "Escape" || event.defaultPrevented) return;
    event.preventDefault();
    onDismiss();
  }

  function listen(active) {
    const method = active ? "addEventListener" : "removeEventListener";
    document[method]("pointerdown", onPointerDown, true);
    document[method]("pointerup", onPointerUp, true);
    document[method]("keydown", onKeyDown, true);
  }

  return {
    element: root,
    show,
    hide,
    reposition: () => place(),
    isOpen: () => open,
  };
}

function sameSize(a, b) {
  return Math.abs(a.width - b.width) < 1 && Math.abs(a.height - b.height) < 1;
}

// --- rows --------------------------------------------------------------------

// Keeps `list` holding one node per item in order, reusing each key's row: while `animate`
// allows, a new row opens in place, a gone one closes out, and the rest slide from where they were.
function syncRows(list, items, rows, { keyOf, create, update, animate }) {
  const before = new Map();
  if (animate)
    for (const [key, row] of rows)
      if (!leavingRows.has(row.node)) before.set(key, row.node.getBoundingClientRect().top);
  const kept = new Set(items.map(keyOf));
  for (const [key, row] of rows)
    if (!kept.has(key) && !leavingRows.has(row.node)) {
      if (!animate) {
        row.node.remove();
        rows.delete(key);
      } else
        closeRow(row, () => {
          if (rows.get(key) !== row || !leavingRows.has(row.node)) return;
          row.node.remove();
          rows.delete(key);
        });
    }
  let cursor = list.firstChild;
  for (const item of items) {
    const key = keyOf(item);
    let row = rows.get(key);
    if (row && leavingRows.has(row.node)) revive(row);
    const fresh = !row;
    if (fresh) {
      row = create(item);
      rows.set(key, row);
    }
    update(row, item);
    while (cursor && cursor !== row.node && leavingRows.has(cursor)) cursor = cursor.nextSibling;
    if (cursor === row.node) cursor = cursor.nextSibling;
    else list.insertBefore(row.node, cursor);
    if (fresh && animate) openRow(row);
  }
  if (!animate) return;
  for (const [key, top] of before) {
    const row = rows.get(key);
    if (!row || leavingRows.has(row.node)) continue;
    // `top` included a slide still running, so the new place is read without it.
    row.slide?.cancel();
    const shift = top - row.node.getBoundingClientRect().top;
    if (Math.abs(shift) < 0.5) continue;
    row.slide = row.node.animate(
      [{ transform: `translateY(${shift}px)` }, { transform: "translateY(0)" }],
      { duration: motionMs("--motion-base"), easing: motionEase("--ease-out") },
    );
  }
}

// A row opens to its height and fades in, clipped while it grows.
function openRow(row) {
  const height = row.node.offsetHeight;
  row.node.style.overflow = "clip";
  row.motion?.cancel();
  row.motion = row.node.animate(
    [
      { height: "0px", opacity: 0 },
      { height: `${height}px`, opacity: 1 },
    ],
    { duration: motionMs("--motion-base"), easing: motionEase("--ease-out") },
  );
  track(row.motion, () => row.node.style.removeProperty("overflow"));
}

function closeRow(row, after) {
  leavingRows.add(row.node);
  row.node.inert = true;
  row.node.style.overflow = "clip";
  row.motion?.cancel();
  row.motion = row.node.animate(
    [
      { height: `${row.node.offsetHeight}px`, opacity: 1 },
      { height: "0px", opacity: 0 },
    ],
    { duration: motionMs("--motion-base"), easing: motionEase("--ease-in-out"), fill: "forwards" },
  );
  track(row.motion, after);
}

// A row asked back while it closes reopens from the height and fade it reached.
function revive(row) {
  leavingRows.delete(row.node);
  row.node.inert = false;
  const height = row.node.getBoundingClientRect().height;
  const { opacity } = getComputedStyle(row.node);
  row.motion?.cancel();
  row.node.style.overflow = "clip";
  row.motion = row.node.animate(
    [
      { height: `${height}px`, opacity },
      { height: `${row.node.offsetHeight}px`, opacity: 1 },
    ],
    { duration: motionMs("--motion-base"), easing: motionEase("--ease-out") },
  );
  track(row.motion, () => row.node.style.removeProperty("overflow"));
}

function track(motion, after) {
  movingRows.add(motion);
  motion.finished.then(after, () => {}).finally(() => movingRows.delete(motion));
}

function setText(node, value) {
  if (node.textContent !== value) node.textContent = value;
}

// --- saved arrangements ------------------------------------------------------

// The name field over the saved arrangements, newest first. A row loads its arrangement when
// pressed; Overwrite and Delete show on hover, and Delete asks once more in place before it acts.
export function createSavedPage(actions) {
  const count = el("span", { className: "canvas-menu-count" });
  const head = el("div", {
    className: "canvas-menu-head",
    children: [el("strong", { text: "Saved arrangements" }), count],
  });
  const input = el("input", {
    className: "input",
    attrs: {
      type: "text",
      id: "arrangement-name",
      placeholder: "Name the arrangement shown",
      maxlength: MAX_ARRANGEMENT_NAME,
      autocomplete: "off",
      "aria-label": "Name for the current arrangement",
    },
  });
  const save = button("", { size: "sm", focusKey: "arrangement-save", onClick: () => submit() });
  const note = el("p", { className: "canvas-menu-note" });
  const list = el("div", {
    className: "canvas-menu-list",
    attrs: { role: "list", "aria-label": "Saved arrangements" },
  });
  const empty = el("p", {
    className: "canvas-menu-empty",
    text: "MonHop remembers each arrangement you apply. Name one above to switch back to it later.",
  });
  const element = el("div", {
    className: "canvas-menu-page",
    dataset: { page: "saved" },
    children: [
      head,
      el("div", { className: "canvas-menu-save", children: [input, save] }),
      note,
      list,
      empty,
    ],
  });
  const rows = new Map();
  let savable = false;
  let armed = null;
  let tones = {};
  let busy = false;
  let names = new Set();
  let replacing = null;
  syncSave();

  input.addEventListener("input", syncSave);
  input.addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    event.preventDefault();
    submit();
  });
  // Any other press takes back a Delete waiting for its second press.
  element.addEventListener("pointerdown", (event) => {
    if (armed && !event.target.closest?.(".saved-row-delete")) arm(null);
  });

  // Saving under a listed name replaces that arrangement, and the button says so.
  function syncSave() {
    const name = normalizeArrangementName(input.value);
    save.disabled = !savable || !name;
    const replaces = names.has(name);
    if (replaces === replacing) return;
    replacing = replaces;
    save.replaceChildren(stableLabel("arrangement-save", ["Save", "Replace"], replaces ? 1 : 0));
  }

  function submit() {
    const name = normalizeArrangementName(input.value);
    if (!name || save.disabled) {
      input.focus({ preventScroll: true });
      return;
    }
    input.value = "";
    syncSave();
    void actions().saveArrangement(name);
  }

  function arm(name) {
    armed = name;
    for (const row of rows.values()) markArmed(row);
  }

  function markArmed(row) {
    const on = row.item?.name === armed;
    if (on) row.node.dataset.armed = "true";
    else delete row.node.dataset.armed;
    row.remove.dataset.variant = on ? "destructive" : "ghost";
    row.remove.setAttribute("aria-pressed", String(on));
    row.remove.setAttribute(
      "aria-label",
      on ? `Delete ${row.item.name} for good` : `Delete ${row.item.name}`,
    );
  }

  function createRow() {
    const thumb = el("span", { className: "saved-thumb-frame" });
    const name = el("strong", { className: "saved-row-name" });
    const crossings = el("span", { className: "saved-row-crossings" });
    const chips = el("span", { className: "saved-row-chips" });
    const meta = el("span", {
      className: "saved-row-meta",
      children: [
        el("span", { className: "saved-row-info", children: [chips, crossings] }),
        el("span", { className: "saved-row-confirm", text: "Press delete again to remove it." }),
      ],
    });
    const main = el("button", {
      className: "saved-row-main",
      attrs: { type: "button" },
      children: [thumb, el("span", { className: "saved-row-copy", children: [name, meta] })],
    });
    const overwrite = tool("refresh-cw", "saved-row-overwrite");
    const remove = tool("trash-2", "saved-row-delete");
    const node = el("div", {
      className: "saved-row",
      attrs: { role: "listitem" },
      children: [main, el("div", { className: "saved-row-tools", children: [overwrite, remove] })],
    });
    const row = { node, main, thumb, name, chips, crossings, overwrite, remove, item: null };
    // Loading shows the arrangement on the canvas, so the menu steps aside for it.
    main.addEventListener("click", () => {
      if (!row.item?.loadable) return;
      actions().loadSavedArrangement(row.item.name);
      actions().closeCanvasMenu();
    });
    overwrite.addEventListener("click", () => {
      if (row.item?.overwritable) void actions().saveArrangement(row.item.name);
    });
    remove.addEventListener("click", () => {
      if (!row.item || busy) return;
      if (armed !== row.item.name) {
        arm(row.item.name);
        return;
      }
      arm(null);
      void actions().deleteArrangement(row.item.name);
    });
    return row;
  }

  function updateRow(row, item) {
    row.item = item;
    setText(row.name, item.name);
    row.name.title = item.name;
    setText(row.crossings, `${item.crossings} crossing${item.crossings === 1 ? "" : "s"}`);
    const chipKey = JSON.stringify([item.entry.automatic, item.entry.fits]);
    if (row.chipKey !== chipKey) {
      row.chipKey = chipKey;
      row.chips.replaceChildren(layoutChipStrip(`${item.key}-menu`, item.entry));
    }
    const drawn = thumbnailRects(item.tiles ?? [], THUMB_BOX, THUMB_INSET);
    const thumbKey = JSON.stringify([drawn, tones]);
    if (row.thumbKey !== thumbKey) {
      row.thumbKey = thumbKey;
      row.thumb.replaceChildren(thumbnail(drawn, tones));
    }
    row.main.disabled = !item.loadable;
    row.main.dataset.focusKey = `${item.key}-load`;
    row.main.setAttribute("aria-label", `Load ${item.name}`);
    row.overwrite.disabled = !item.overwritable;
    row.overwrite.dataset.focusKey = `${item.key}-overwrite`;
    row.overwrite.setAttribute("aria-label", `Overwrite ${item.name} with the arrangement shown`);
    row.overwrite.title = "Overwrite with the arrangement shown";
    row.remove.disabled = busy;
    row.remove.dataset.focusKey = `${item.key}-delete`;
    row.remove.title = "Delete";
  }

  return {
    element,
    update(next) {
      busy = next.busy;
      savable = next.savable;
      tones = next.tones;
      names = new Set(next.items.map((item) => item.name));
      syncSave();
      input.disabled = !savable;
      setText(count, next.items.length ? String(next.items.length) : "");
      // The hint only says what blocks saving; saving under a listed name replaces it.
      note.hidden = next.busy || next.canSave;
      setText(note, "Arrange the displays so they touch, then save them under a name.");
      if (armed && !next.items.some((item) => item.name === armed)) armed = null;
      syncRows(list, next.items, rows, {
        keyOf: (item) => item.name,
        create: createRow,
        update: updateRow,
        animate: next.animate,
      });
      for (const row of rows.values()) if (row.item) markArmed(row);
      empty.hidden = next.items.length > 0;
    },
    close() {
      if (armed) arm(null);
    },
  };
}

function tool(iconName, className) {
  const node = button("", { variant: "ghost", size: "sm", iconName });
  node.classList.add("saved-row-tool", className);
  return node;
}

// The arrangement loading would draw, each display in its computer's color; a dashed frame when it
// no longer places on the displays connected now.
function thumbnail(rects, tones) {
  const svg = svgNode("svg");
  svg.classList.add("saved-thumb");
  svg.setAttribute("viewBox", `0 0 ${THUMB_BOX.width} ${THUMB_BOX.height}`);
  svg.setAttribute("aria-hidden", "true");
  if (!rects.length) {
    const frame = svgNode("rect");
    frame.classList.add("saved-thumb-missing");
    for (const [name, value] of Object.entries({ x: 2, y: 4, width: 40, height: 22, rx: 3 }))
      frame.setAttribute(name, String(value));
    svg.append(frame);
    return svg;
  }
  for (const box of rects) {
    const screen = svgNode("rect");
    screen.classList.add("saved-thumb-screen");
    screen.dataset.tone = tones[box.group] ?? "local";
    for (const name of ["x", "y", "width", "height"]) screen.setAttribute(name, String(box[name]));
    screen.setAttribute("rx", "1.5");
    svg.append(screen);
  }
  return svg;
}

// --- displays ----------------------------------------------------------------

let detailCount = 0;

function createSection(data) {
  const label = el("span");
  const rowsBox = el("div", {
    className: "canvas-menu-list",
    attrs: { role: "group" },
  });
  const node = el("section", {
    className: "canvas-menu-section",
    children: [
      el("div", {
        className: "canvas-menu-section-head",
        children: [el("span", { className: "canvas-menu-swatch" }), label],
      }),
      rowsBox,
    ],
  });
  return { node, label, rowsBox, rows: new Map(), key: data.key };
}

// Each computer's displays with a switch apiece; off leaves a display out of the picture and every
// route. Opened from a display it lists that display's computer, with that display marked.
export function createDisplaysPage(actions) {
  const swatch = el("span", { className: "canvas-menu-swatch" });
  const title = el("strong");
  const head = el("div", { className: "canvas-menu-head", children: [swatch, title] });
  const sectionsBox = el("div", { className: "canvas-menu-sections" });
  const element = el("div", {
    className: "canvas-menu-page",
    dataset: { page: "displays" },
    children: [
      head,
      sectionsBox,
      el("p", { className: "canvas-menu-note", text: "The pointer skips displays you turn off." }),
    ],
  });
  const sections = new Map();
  let busy = false;

  function updateSection(section, data, animate) {
    section.node.dataset.tone = data.tone;
    setText(section.label, data.label);
    section.rowsBox.setAttribute("aria-label", `${data.label} displays`);
    syncRows(section.rowsBox, data.rows, section.rows, {
      keyOf: (row) => row.id,
      create: createRow,
      update: updateRow,
      animate,
    });
  }

  function createRow() {
    const name = el("strong");
    const detail = el("small", { attrs: { id: `display-row-detail-${++detailCount}` } });
    const control = el("button", {
      className: "switch",
      attrs: { type: "button", role: "switch", "aria-describedby": detail.id },
      children: [el("span", { className: "switch-thumb", attrs: { "aria-hidden": "true" } })],
    });
    const node = el("label", {
      className: "display-row",
      children: [
        el("span", {
          className: "display-glyph",
          attrs: { "aria-hidden": "true" },
          children: [el("span", { className: "display-glyph-screen" })],
        }),
        el("span", { className: "display-row-copy", children: [name, detail] }),
        control,
      ],
    });
    const row = { node, name, detail, control, item: null };
    control.addEventListener("click", () => {
      if (row.item && !control.disabled) actions().useDisplay(row.item.id, !row.item.inUse);
    });
    return row;
  }

  function updateRow(row, item) {
    row.item = item;
    setText(row.name, item.name);
    setText(row.detail, item.detail);
    row.node.dataset.useDisplay = item.id;
    row.node.dataset.inUse = String(item.inUse);
    row.node.dataset.selected = String(item.selected);
    row.node.style.setProperty("--display-aspect", String(item.aspect));
    row.control.setAttribute("aria-checked", String(item.inUse));
    row.control.setAttribute("aria-label", `Use ${item.name}`);
    row.control.dataset.focusKey = `arrangement-use-${item.id}`;
    row.control.disabled = busy || item.locked;
  }

  return {
    element,
    update(next) {
      busy = next.busy;
      element.dataset.scoped = String(next.scoped);
      const only = next.scoped ? next.sections[0] : null;
      if (only) swatch.dataset.tone = only.tone;
      swatch.hidden = !only;
      setText(title, only ? `${only.label} displays` : "Displays in use");
      const seen = new Set();
      let cursor = sectionsBox.firstChild;
      for (const data of next.sections) {
        seen.add(data.key);
        let section = sections.get(data.key);
        if (!section) {
          section = createSection(data);
          sections.set(data.key, section);
        }
        section.node.hidden = false;
        updateSection(section, data, next.animate);
        if (cursor === section.node) cursor = cursor.nextSibling;
        else sectionsBox.insertBefore(section.node, cursor);
      }
      for (const [key, section] of sections) if (!seen.has(key)) section.node.hidden = true;
    },
  };
}
