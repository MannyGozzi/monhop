const TITLES = { ready: "Get ready", computers: "Computers", displays: "Displays" };
const MID_EXCHANGE = new Set([
  "identity-missing",
  "review",
  "requesting-network",
  "waiting",
  "connecting",
  "saving",
  "stopping",
  "error",
]);

// Setup is one page whose sections open in order: each one states in its own header why it is still shut.
export function setupSectionGates({
  readyDone,
  pairedCount = 0,
  activeName = "",
  displaysReady,
  layoutSaved,
} = {}) {
  const ready = gate("ready", 0, { done: readyDone === true, locked: false, reason: "" });
  const computers = gate("computers", 1, {
    done: pairedCount > 0,
    locked: !ready.done,
    reason: "Finish Get ready first",
  });
  const displays = gate("displays", 2, {
    done: layoutSaved === true,
    locked: computers.locked || !computers.done || displaysReady !== true,
    reason: displaysLockReason(computers, activeName),
  });
  return { ready, computers, displays };
}

function displaysLockReason(computers, activeName) {
  if (computers.locked) return "Finish Get ready first";
  if (!computers.done) return "Pair a computer first";
  if (!activeName) return "Choose a computer to use";
  return `Waiting for ${activeName}`;
}

// Once a computer is paired the list is the focus: the code exchange opens on request, or by itself
// while an exchange is under way.
export function shouldShowPairing({ requested, pairedCount = 0, phase } = {}) {
  return requested === true || pairedCount === 0 || MID_EXCHANGE.has(phase);
}

function gate(key, index, { done, locked, reason }) {
  return {
    key,
    index,
    title: TITLES[key],
    done: done && !locked,
    locked,
    reason: locked ? reason : "",
  };
}
