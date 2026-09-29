// Pure ordering for Home's list of computer cards, kept apart from the DOM so the split between the
// group picture, the computers switched in and the rest is checked without a browser.
import { computerTone, resolveEnabledList } from "./computer-card-model.mjs";

// Home's cards, top to bottom: the group arrangement card once more than one computer is switched
// in, then one entry per computer switched in (in the group's own order, so its tone matches the
// picture above it), then every other paired computer. With zero or one switched in there is no
// group entry: the lone enabled computer, if any, comes back as a "hero" entry — the same single
// card Home has always drawn for it — so a caller that only ever sees zero or one renders exactly
// as it always has.
export function homeEntries(computers, sharingView, active) {
  const enabledList = resolveEnabledList(sharingView, computers, active);
  const enabledSet = new Set(enabledList);
  const order = new Map(enabledList.map((fingerprint, index) => [fingerprint, index]));
  const enabled = computers.items
    .filter((item) => enabledSet.has(item.fingerprint))
    .toSorted((a, b) => order.get(a.fingerprint) - order.get(b.fingerprint));
  const others = computers.items.filter((item) => !enabledSet.has(item.fingerprint));

  const entries = [];
  if (enabled.length > 1) entries.push({ type: "group" });
  const liveType = enabled.length > 1 ? "live" : "hero";
  for (const computer of enabled)
    entries.push({
      type: liveType,
      computer,
      tone: computerTone(computer.fingerprint, enabledList),
    });
  for (const computer of others) entries.push({ type: "other", computer });
  return entries;
}
