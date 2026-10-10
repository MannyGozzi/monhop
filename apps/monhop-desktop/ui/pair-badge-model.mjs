// The match badge: the backend sends indices into these lists, so both computers must keep the
// same order across versions. Never reorder or replace an entry.
export const PAIR_BADGE_COLORS = Object.freeze([
  { key: "violet", name: "Violet" },
  { key: "blue", name: "Blue" },
  { key: "sky", name: "Sky" },
  { key: "teal", name: "Teal" },
  { key: "lime", name: "Lime" },
  { key: "brown", name: "Brown" },
  { key: "pink", name: "Pink" },
  { key: "slate", name: "Slate" },
]);

// Lucide icons (icons.mjs) with one everyday name each.
export const PAIR_BADGE_SYMBOLS = Object.freeze([
  { icon: "anchor", name: "anchor" },
  { icon: "apple", name: "apple" },
  { icon: "bell", name: "bell" },
  { icon: "bike", name: "bike" },
  { icon: "bird", name: "bird" },
  { icon: "book", name: "book" },
  { icon: "camera", name: "camera" },
  { icon: "car", name: "car" },
  { icon: "cat", name: "cat" },
  { icon: "cloud", name: "cloud" },
  { icon: "coffee", name: "cup" },
  { icon: "crown", name: "crown" },
  { icon: "fish", name: "fish" },
  { icon: "flag", name: "flag" },
  { icon: "flower", name: "flower" },
  { icon: "gift", name: "gift" },
  { icon: "heart", name: "heart" },
  { icon: "house", name: "house" },
  { icon: "key", name: "key" },
  { icon: "leaf", name: "leaf" },
  { icon: "lightbulb", name: "light bulb" },
  { icon: "moon", name: "moon" },
  { icon: "mountain", name: "mountain" },
  { icon: "music", name: "music note" },
  { icon: "plane", name: "plane" },
  { icon: "rocket", name: "rocket" },
  { icon: "snowflake", name: "snowflake" },
  { icon: "star", name: "star" },
  { icon: "sun", name: "sun" },
  { icon: "tree-pine", name: "tree" },
  { icon: "umbrella", name: "umbrella" },
  { icon: "zap", name: "lightning" },
]);

const SYMBOL_COUNT = 3;

function isIndex(value, length) {
  return Number.isInteger(value) && value >= 0 && value < length;
}

// `{ color, symbols: [a, b, c] }` with every index in range, or null.
export function normalizePairBadge(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const { color, symbols } = value;
  if (
    !isIndex(color, PAIR_BADGE_COLORS.length) ||
    !Array.isArray(symbols) ||
    symbols.length !== SYMBOL_COUNT ||
    !symbols.every((symbol) => isIndex(symbol, PAIR_BADGE_SYMBOLS.length))
  )
    return null;
  return { color, symbols: [...symbols] };
}

export function pairBadgeParts(badge) {
  return {
    color: PAIR_BADGE_COLORS[badge.color],
    symbols: badge.symbols.map((index) => PAIR_BADGE_SYMBOLS[index]),
  };
}

// What a screen reader says, and what people read aloud to compare: "Violet: anchor, moon, leaf".
export function pairBadgeLabel(badge) {
  const { color, symbols } = pairBadgeParts(badge);
  return `${color.name}: ${symbols.map((symbol) => symbol.name).join(", ")}`;
}
