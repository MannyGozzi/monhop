import assert from "node:assert/strict";
import test from "node:test";

import {
  canCheck,
  canInstall,
  installHint,
  normalizeUpdatesView,
  updatesStatusText,
} from "./updates-model.mjs";

const base = {
  automatic: true,
  phase: "idle",
  currentVersion: "0.1.0",
  buildCommit: "03b045812",
  availableVersion: null,
  notes: null,
  progressPercent: null,
  message: "",
  checkedSecondsAgo: null,
  host: "github.com",
  sharingActive: false,
};

test("a full view normalizes to itself", () => {
  assert.deepEqual(normalizeUpdatesView(base), base);
});

const defaults = { ...base, currentVersion: "", buildCommit: "" };

test("missing or malformed fields fall back to safe defaults", () => {
  assert.deepEqual(normalizeUpdatesView(null), defaults);
  assert.deepEqual(normalizeUpdatesView({}), defaults);
  assert.equal(normalizeUpdatesView({ phase: "mid-flight" }).phase, "idle");
  assert.equal(normalizeUpdatesView({ automatic: "yes" }).automatic, true);
  assert.equal(normalizeUpdatesView({ currentVersion: 12 }).currentVersion, "");
  assert.equal(normalizeUpdatesView({ host: 12 }).host, "github.com");
});

test("automatic is on unless the view says otherwise", () => {
  assert.equal(normalizeUpdatesView({ automatic: false }).automatic, false);
  assert.equal(normalizeUpdatesView({}).automatic, true);
});

test("numbers are clamped and out-of-range strings become null", () => {
  assert.equal(normalizeUpdatesView({ progressPercent: 140 }).progressPercent, 100);
  assert.equal(normalizeUpdatesView({ progressPercent: -5 }).progressPercent, 0);
  assert.equal(normalizeUpdatesView({ progressPercent: 42.6 }).progressPercent, 43);
  assert.equal(normalizeUpdatesView({ progressPercent: "nope" }).progressPercent, null);
  assert.equal(normalizeUpdatesView({ checkedSecondsAgo: -30 }).checkedSecondsAgo, 0);
  assert.equal(normalizeUpdatesView({ checkedSecondsAgo: 90 }).checkedSecondsAgo, 90);
});

test("status text follows the phase", () => {
  assert.equal(updatesStatusText({ ...base, phase: "idle" }), "Not checked yet.");
  assert.equal(
    updatesStatusText({ ...base, phase: "idle", checkedSecondsAgo: 125 }),
    "Checked 2 minutes ago.",
  );
  assert.equal(
    updatesStatusText({ ...base, phase: "idle", checkedSecondsAgo: 5 }),
    "Checked 1 minutes ago.",
  );
  assert.equal(updatesStatusText({ ...base, phase: "checking" }), "Checking github.com…");
  assert.equal(updatesStatusText({ ...base, phase: "upToDate" }), "MonHop is up to date.");
  assert.equal(
    updatesStatusText({ ...base, phase: "available", availableVersion: "0.2.0", automatic: true }),
    "MonHop 0.2.0 is available. Downloading…",
  );
  assert.equal(
    updatesStatusText({
      ...base,
      phase: "available",
      availableVersion: "0.2.0",
      automatic: false,
    }),
    "MonHop 0.2.0 is available.",
  );
  assert.equal(
    updatesStatusText({
      ...base,
      phase: "downloading",
      availableVersion: "0.2.0",
      progressPercent: 42,
    }),
    "Downloading MonHop 0.2.0… 42%",
  );
  assert.equal(
    updatesStatusText({ ...base, phase: "downloading", availableVersion: "0.2.0" }),
    "Downloading MonHop 0.2.0… 0%",
  );
  assert.equal(
    updatesStatusText({ ...base, phase: "ready", availableVersion: "0.2.0" }),
    "MonHop 0.2.0 is ready to install.",
  );
  assert.equal(
    updatesStatusText({ ...base, phase: "failed", message: "No network." }),
    "No network.",
  );
  assert.equal(
    updatesStatusText({ ...base, phase: "failed", message: "" }),
    "The update check did not finish.",
  );
});

test("checking is allowed unless a check or download is already running", () => {
  assert.equal(canCheck({ ...base, phase: "idle" }), true);
  assert.equal(canCheck({ ...base, phase: "upToDate" }), true);
  assert.equal(canCheck({ ...base, phase: "available" }), true);
  assert.equal(canCheck({ ...base, phase: "ready" }), true);
  assert.equal(canCheck({ ...base, phase: "failed" }), true);
  assert.equal(canCheck({ ...base, phase: "checking" }), false);
  assert.equal(canCheck({ ...base, phase: "downloading" }), false);
});

test("installing needs a ready build and no active sharing session", () => {
  assert.equal(canInstall({ ...base, phase: "ready" }), true);
  assert.equal(canInstall({ ...base, phase: "ready", sharingActive: true }), false);
  assert.equal(canInstall({ ...base, phase: "downloading" }), false);
  assert.equal(canInstall({ ...base, phase: "idle" }), false);
});

test("the install hint only appears when sharing blocks a ready install", () => {
  assert.equal(
    installHint({ ...base, phase: "ready", sharingActive: true }),
    "Sharing is running. Turn it off first or quit MonHop to update.",
  );
  assert.equal(installHint({ ...base, phase: "ready", sharingActive: false }), "");
  assert.equal(installHint({ ...base, phase: "downloading", sharingActive: true }), "");
});
