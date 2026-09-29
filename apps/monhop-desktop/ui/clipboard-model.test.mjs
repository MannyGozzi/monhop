import assert from "node:assert/strict";
import test from "node:test";

import {
  CLIPBOARD_NEEDS_CONNECTION,
  clipboardAccessNotice,
  clipboardNoticeText,
  clipboardPeerLines,
  clipboardStatusText,
  lastTransferText,
  normalizeClipboardView,
} from "./clipboard-model.mjs";

const FP_A = "a".repeat(64);
const FP_B = "b".repeat(64);
const FP_C = "c".repeat(64);

const computers = {
  items: [
    { fingerprint: FP_A, platform: "macos", name: "Studio" },
    { fingerprint: FP_B, platform: "windows", name: "" },
  ],
};

// ---------- normalizeClipboardView ----------

test("a missing or garbage payload normalizes to a safe, empty view instead of throwing", () => {
  for (const bogus of [undefined, null, "nope", 42, [], true]) {
    assert.deepEqual(normalizeClipboardView(bogus), {
      enabled: false,
      access: "unknown",
      notice: null,
      peers: [],
      last: null,
    });
  }
});

test("enabled coerces to a strict boolean", () => {
  assert.equal(normalizeClipboardView({ enabled: true }).enabled, true);
  assert.equal(normalizeClipboardView({ enabled: "true" }).enabled, false);
  assert.equal(normalizeClipboardView({ enabled: 1 }).enabled, false);
});

test("an unrecognized access value falls back to unknown", () => {
  for (const access of ["allowed", "ask", "denied", "unknown"])
    assert.equal(normalizeClipboardView({ access }).access, access);
  for (const access of ["Allowed", "granted", "", null, undefined, 3])
    assert.equal(normalizeClipboardView({ access }).access, "unknown");
});

test("an unrecognized notice falls back to null", () => {
  for (const notice of ["tooLarge", "unsupported", "accessDenied"])
    assert.equal(normalizeClipboardView({ notice }).notice, notice);
  for (const notice of ["TooLarge", "other", "", 5])
    assert.equal(normalizeClipboardView({ notice }).notice, null);
  assert.equal(normalizeClipboardView({ notice: null }).notice, null);
  assert.equal(normalizeClipboardView({}).notice, null);
});

test("peers are bounded, deduplicated, and keep only valid hex fingerprints", () => {
  const many = Array.from({ length: 12 }, (_, i) => ({
    fingerprint: i.toString(16).repeat(64).slice(0, 64),
    peerEnabled: true,
  }));
  assert.equal(normalizeClipboardView({ peers: many }).peers.length, 8);

  const view = normalizeClipboardView({
    peers: [
      { fingerprint: FP_A, peerEnabled: true },
      { fingerprint: FP_A, peerEnabled: false }, // duplicate, dropped
      { fingerprint: "not-hex", peerEnabled: true },
      { fingerprint: FP_A.toUpperCase(), peerEnabled: true }, // same peer, different case
      { fingerprint: FP_B, peerEnabled: "yes" }, // unrecognized -> null tri-state
      { fingerprint: FP_C, peerEnabled: undefined },
      null,
      "garbage",
    ],
  });
  assert.deepEqual(view.peers, [
    { fingerprint: FP_A, peerEnabled: true },
    { fingerprint: FP_B, peerEnabled: null },
    { fingerprint: FP_C, peerEnabled: null },
  ]);
  assert.equal(normalizeClipboardView({ peers: "nope" }).peers.length, 0);
});

test("a last transfer missing an identifying field is dropped rather than guessed", () => {
  const base = { direction: "sent", kind: "text", peer: FP_A, bytes: 10, ageSeconds: 5 };
  assert.deepEqual(normalizeClipboardView({ last: base }).last, base);
  for (const bad of [
    { ...base, direction: "uploaded" },
    { ...base, kind: "video" },
    { ...base, peer: "not-hex" },
    "garbage",
    42,
    null,
  ])
    assert.equal(normalizeClipboardView({ last: bad }).last, null);
});

test("bytes and age on the last transfer are clamped, never negative or NaN", () => {
  const base = { direction: "sent", kind: "text", peer: FP_A };
  assert.equal(
    normalizeClipboardView({ last: { ...base, bytes: -5, ageSeconds: -1 } }).last.bytes,
    0,
  );
  assert.equal(
    normalizeClipboardView({ last: { ...base, bytes: "NaN", ageSeconds: "x" } }).last.bytes,
    0,
  );
  assert.equal(normalizeClipboardView({ last: { ...base, bytes: 12.6 } }).last.bytes, 13);
});

test("a malicious payload can never smuggle clipboard content through the normalized view", () => {
  const secret = "hunter2 super secret clipboard text";
  const malicious = {
    enabled: true,
    content: secret,
    text: secret,
    clipboardText: secret,
    peers: [{ fingerprint: FP_A, peerEnabled: true, content: secret }],
    last: {
      direction: "sent",
      kind: "text",
      peer: FP_A,
      bytes: 4,
      ageSeconds: 1,
      content: secret,
      preview: secret,
    },
  };
  const view = normalizeClipboardView(malicious);
  const serialized = JSON.stringify(view);
  assert.ok(!serialized.includes(secret));
  assert.deepEqual(Object.keys(view).toSorted(), ["access", "enabled", "last", "notice", "peers"]);
  assert.deepEqual(Object.keys(view.peers[0]).toSorted(), ["fingerprint", "peerEnabled"]);
  assert.deepEqual(Object.keys(view.last).toSorted(), [
    "ageSeconds",
    "bytes",
    "direction",
    "kind",
    "peer",
  ]);
});

// ---------- status text ----------

test("the headline status reads off, waiting, or on depending on enabled and attached peers", () => {
  assert.equal(clipboardStatusText(normalizeClipboardView({ enabled: false })), "Off.");
  assert.equal(
    clipboardStatusText(normalizeClipboardView({ enabled: true })),
    "On. Waiting for a connected computer.",
  );
  assert.equal(
    clipboardStatusText(
      normalizeClipboardView({ enabled: true, peers: [{ fingerprint: FP_A, peerEnabled: true }] }),
    ),
    "On.",
  );
});

test("peer lines name each attached computer and say whether it needs turning on there too", () => {
  const view = normalizeClipboardView({
    enabled: true,
    peers: [
      { fingerprint: FP_A, peerEnabled: true },
      { fingerprint: FP_B, peerEnabled: false },
      { fingerprint: FP_C, peerEnabled: null }, // unreported yet reads the same as off
    ],
  });
  assert.deepEqual(clipboardPeerLines(view, computers), [
    "Studio: on",
    "Windows PC: off. Turn it on there too",
    "Windows PC: off. Turn it on there too",
  ]);
});

test("peer lines never throw when the computers list is missing or empty", () => {
  const view = normalizeClipboardView({
    enabled: true,
    peers: [{ fingerprint: FP_A, peerEnabled: true }],
  });
  assert.deepEqual(clipboardPeerLines(view, undefined), ["Windows PC: on"]);
  assert.deepEqual(clipboardPeerLines(view, { items: [] }), ["Windows PC: on"]);
});

test("the macOS access notice covers ask and denied, and is silent otherwise", () => {
  assert.match(
    clipboardAccessNotice(normalizeClipboardView({ access: "ask" })),
    /Allow MonHop in System Settings/,
  );
  assert.match(
    clipboardAccessNotice(normalizeClipboardView({ access: "denied" })),
    /Allow MonHop in System Settings/,
  );
  assert.equal(clipboardAccessNotice(normalizeClipboardView({ access: "allowed" })), null);
  assert.equal(clipboardAccessNotice(normalizeClipboardView({ access: "unknown" })), null);
});

test("the last-copy notice covers every skip reason and is silent when there is none", () => {
  assert.match(clipboardNoticeText(normalizeClipboardView({ notice: "tooLarge" })), /too large/);
  assert.match(
    clipboardNoticeText(normalizeClipboardView({ notice: "unsupported" })),
    /format MonHop can share/,
  );
  assert.match(
    clipboardNoticeText(normalizeClipboardView({ notice: "accessDenied" })),
    /doesn't have clipboard access/,
  );
  assert.equal(clipboardNoticeText(normalizeClipboardView({})), null);
});

test("the connection note explains that clipboard sharing rides on a control connection", () => {
  assert.match(CLIPBOARD_NEEDS_CONNECTION, /control/);
  assert.match(CLIPBOARD_NEEDS_CONNECTION, /clipboard/);
});

// ---------- last transfer text and size formatting ----------

test("the last transfer reads as direction, kind, human size, and age — nothing else", () => {
  const view = (overrides) =>
    normalizeClipboardView({
      last: {
        direction: "sent",
        kind: "image",
        peer: FP_A,
        bytes: 0,
        ageSeconds: 12,
        ...overrides,
      },
    });
  assert.equal(lastTransferText(view({})), "Sent an image (0 B) 12 s ago");
  assert.equal(
    lastTransferText(view({ direction: "received", kind: "text", bytes: 1234, ageSeconds: 5 })),
    "Received text (1.2 KB) 5 s ago",
  );
  assert.equal(
    lastTransferText(view({ bytes: Math.round(1.4 * 1024 * 1024), ageSeconds: 12 })),
    "Sent an image (1.4 MB) 12 s ago",
  );
  assert.equal(lastTransferText(normalizeClipboardView({})), null);
});

test("size formatting steps from bytes to KB to MB to GB", () => {
  const at = (bytes) =>
    lastTransferText(
      normalizeClipboardView({
        last: { direction: "sent", kind: "text", peer: FP_A, bytes, ageSeconds: 0 },
      }),
    );
  assert.equal(at(999), "Sent text (999 B) 0 s ago");
  assert.equal(at(1000), "Sent text (1000 B) 0 s ago");
  assert.equal(at(1024), "Sent text (1 KB) 0 s ago");
  assert.equal(at(15 * 1024), "Sent text (15 KB) 0 s ago");
  assert.equal(at(1024 * 1024), "Sent text (1 MB) 0 s ago");
  assert.equal(at(1024 * 1024 * 1024), "Sent text (1 GB) 0 s ago");
});

test("age formatting steps from seconds to minutes to hours", () => {
  const at = (ageSeconds) =>
    lastTransferText(
      normalizeClipboardView({
        last: { direction: "sent", kind: "text", peer: FP_A, bytes: 1, ageSeconds },
      }),
    );
  assert.equal(at(0), "Sent text (1 B) 0 s ago");
  assert.equal(at(59), "Sent text (1 B) 59 s ago");
  assert.equal(at(60), "Sent text (1 B) 1 min ago");
  assert.equal(at(125), "Sent text (1 B) 2 min ago");
  assert.equal(at(3600), "Sent text (1 B) 1 h ago");
  assert.equal(at(7300), "Sent text (1 B) 2 h ago");
});
