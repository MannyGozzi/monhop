// Every decision a computer card makes about its arrangement viewport and its Layouts list, kept
// apart from the DOM so the rules are checked without a browser. The card only draws what it returns.

// Focus is restored by key across a re-render, so two rows of one computer must never share one.
// The fingerprint's first bytes name the computer and a hash of the whole name names the row.
export function layoutRowKey(fingerprint, name) {
  return `${String(fingerprint ?? "").slice(0, 8)}-${hash(String(name ?? ""))}`;
}

// FNV-1a over the whole name: stable between renders, and short enough to keep the key well
// under the 80 characters a focus key may use.
function hash(value) {
  let sum = 0x811c9dc5;
  for (let index = 0; index < value.length; index += 1) {
    sum = Math.imul(sum ^ value.charCodeAt(index), 0x01000193) >>> 0;
  }
  return sum.toString(36);
}

// The library hands back its oldest entry first; every surface shows the newest first.
export function newestFirst(entries) {
  return Array.isArray(entries) ? entries.toReversed() : [];
}

// --- displays --------------------------------------------------------------

// The card draws the saved arrangement, so one word under it says whether the link is reporting
// those same displays right now. Both computers must be reporting before it reads as live.
export function displaysFreshness(setup) {
  const live = setup?.live;
  if (counted(live?.localDisplays) && counted(live?.peerDisplays)) return "Live";
  return counted(setup?.localDisplays) || counted(setup?.peerDisplays) ? "Last seen" : null;
}

function counted(value) {
  return Array.isArray(value) && value.length > 0;
}

// --- layout history --------------------------------------------------------

// A remembered layout needs no chip: everything in this list is remembered. Only a name the user
// typed is worth marking, and only an entry that says so is one: Rust always sends the boolean,
// so a missing or malformed one is a damaged entry, not a saved layout. "Fits now" comes and goes
// with the displays, so it is its own slot the card can animate in and out while the marks stay put.
export function layoutChips(entry) {
  return {
    marks: entry?.automatic === false ? [{ tone: "neutral", label: "Saved" }] : [],
    fits: entry?.fits === true ? { tone: "connected", label: "Fits now" } : null,
  };
}

const LOAD_BLOCKED = {
  inactive: "Use this computer to load its layouts.",
  disconnected: "Available once this computer is connected.",
  misfit: "This layout does not fit the displays connected now.",
};

// Load is drawn on every row and enabled only when pressing it would really load: the entry the
// row drew, on the computer in use, over a connected link. A blocked row says why.
export function loadGate({ isActive, connected, entry }) {
  if (!isActive) return { enabled: false, reason: LOAD_BLOCKED.inactive };
  if (!connected) return { enabled: false, reason: LOAD_BLOCKED.disconnected };
  if (entry?.fits !== true || !entry.layout) return { enabled: false, reason: LOAD_BLOCKED.misfit };
  return { enabled: true, reason: "" };
}

// Forget takes two presses. One row across the whole app is armed at a time, named by its
// computer and its exact layout, so the same name under another computer is never armed with it.
export function isForgetArmed(armed, fingerprint, name) {
  return armed?.fingerprint === fingerprint && armed?.name === name;
}

export function pressForget(armed, fingerprint, name) {
  return isForgetArmed(armed, fingerprint, name)
    ? { armed: null, forget: true }
    : { armed: { fingerprint, name }, forget: false };
}

// Reading a computer's list again replaces the entry the armed row named, so the confirm drops.
export function clearForgetFor(armed, fingerprint) {
  return armed?.fingerprint === fingerprint ? null : armed;
}

// A computer that is no longer paired has no rows left to confirm.
export function keepForgetArmed(armed, fingerprints) {
  return armed && [...fingerprints].includes(armed.fingerprint) ? armed : null;
}

// A list read again in the background keeps the confirm while the row it armed is still listed.
export function keepForgetListed(armed, fingerprint, entries) {
  if (armed?.fingerprint !== fingerprint) return armed;
  return entries.some((entry) => entry.name === armed.name) ? armed : null;
}

// --- control switches --------------------------------------------------

export const CONTROL_PAUSE_HINT = "Use Pause to stop sharing";

// A specific member's own "may control the others" toggle, read from the group's per-member list
// (`control.members`, one entry per computer other than this one). Legacy `control.peerToLocal` is
// the fallback: no member list, or no fingerprint to look one up by, so every existing single-peer
// caller reads exactly as it always has.
function memberPeerToLocal(control, fingerprint) {
  if (fingerprint && Array.isArray(control?.members)) {
    const entry = control.members.find((member) => member.fingerprint === fingerprint);
    if (entry) return entry.allowed;
  }
  return control?.peerToLocal ?? true;
}

// Two switches, one per direction. With no active record yet, both read on, matching what a fresh
// setup turns on by default. The last enabled direction cannot be turned off here — pausing is how
// sharing stops entirely — and both disable while a change to either is still syncing. `fingerprint`
// picks one member's own direction out of a group's `control.members`; omitted, this is today's
// single-peer pair.
export function controlSwitchRows(control, localName, peerName, syncing, fingerprint) {
  const localToPeer = control?.localToPeer ?? true;
  const peerToLocal = memberPeerToLocal(control, fingerprint);
  const isSyncing = syncing === true || control?.syncing === true;
  const row = (direction, label, checked, isLast) => ({
    direction,
    label,
    checked,
    disabled: isSyncing || isLast,
    hint: isLast ? CONTROL_PAUSE_HINT : isSyncing ? "Updating both computers…" : "",
  });
  return [
    row(
      "localToPeer",
      `${localName} can control ${peerName}`,
      localToPeer,
      localToPeer && !peerToLocal,
    ),
    row(
      "peerToLocal",
      `${peerName} can control ${localName}`,
      peerToLocal,
      peerToLocal && !localToPeer,
    ),
  ];
}

// One pass over a computer's history: order, row keys, the armed confirm, and whether Load works.
// `pending` is that computer's own read or forget still in flight, which locks its rows alone.
export function layoutRows({ fingerprint, entries, armed, isActive, connected, busy, pending }) {
  const locked = busy === true || pending === true;
  return newestFirst(entries).map((entry) => ({
    entry,
    key: layoutRowKey(fingerprint, entry.name),
    armed: isForgetArmed(armed, fingerprint, entry.name),
    disabled: locked,
    load: loadGate({ isActive, connected, entry }),
  }));
}

// --- switching computers in and out -----------------------------------------

// Which computers are switched in, in the group's own order: the live view's own list wins once the
// backend sends one, then the polled computers reply's own list, and a bare `active` fingerprint
// (today's only source) is always the fallback, so a reply from before the multi-computer backend
// lands reads exactly as it always has.
export function resolveEnabledList(sharingView, computers, active) {
  if (Array.isArray(sharingView?.enabled) && sharingView.enabled.length) return sharingView.enabled;
  if (Array.isArray(computers?.enabled) && computers.enabled.length) return computers.enabled;
  return active ? [active] : [];
}

export function isComputerEnabled(fingerprint, enabledList) {
  return Array.isArray(enabledList) && enabledList.includes(fingerprint);
}

// The color slot a card's icon and pill draw in, matching the group arrangement picture's own tones
// (arrangement-view-model.mjs's `defaultTone`, applied to the peers only: the local computer never
// gets its own Home card, so "local" never appears here): the first enabled computer is "peer", the
// rest are "peer-2".."peer-6" (a group tops out at 8 members, so an 8th enabled computer reuses
// "peer-6" rather than drawing in no color at all).
export function computerTone(fingerprint, enabledList) {
  const index = Array.isArray(enabledList) ? enabledList.indexOf(fingerprint) : -1;
  const position = index < 0 ? 0 : index;
  return position === 0 ? "peer" : `peer-${Math.min(position + 1, 6)}`;
}

// Switches one computer in or out: the new per-computer action once the app provides it, falling
// back to today's single-active-computer action so an app.js that has not wired the new one up yet
// keeps working exactly as before.
export function toggleComputerEnabled(actions, fingerprint, enabled) {
  if (typeof actions?.setComputerEnabled === "function")
    return actions.setComputerEnabled(fingerprint, !enabled);
  return actions.useComputer(enabled ? null : fingerprint);
}
