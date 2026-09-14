import assert from "node:assert/strict";
import test from "node:test";

import {
  autostartDescription,
  autostartOpenLabel,
  autostartStatusText,
  normalizeAutostartView,
  showAutostartOpenSettings,
} from "./autostart-model.mjs";

const base = { enabled: true, state: "on", paired: true, message: "" };

test("a full view normalizes to itself", () => {
  assert.deepEqual(normalizeAutostartView(base), base);
});

const defaults = { enabled: true, state: "off", paired: false, message: "" };

test("missing or malformed fields fall back to safe defaults", () => {
  assert.deepEqual(normalizeAutostartView(null), defaults);
  assert.deepEqual(normalizeAutostartView({}), defaults);
  assert.equal(normalizeAutostartView({ state: "mid-flight" }).state, "off");
  assert.equal(normalizeAutostartView({ enabled: "yes" }).enabled, true);
  assert.equal(normalizeAutostartView({ paired: "yes" }).paired, false);
  assert.equal(normalizeAutostartView({ message: 12 }).message, "");
});

test("enabled is on unless the view says otherwise", () => {
  assert.equal(normalizeAutostartView({ enabled: false }).enabled, false);
  assert.equal(normalizeAutostartView({}).enabled, true);
});

test("status text follows the state, word for word", () => {
  assert.equal(autostartStatusText({ ...base, state: "on" }), "Starts at login.");
  assert.equal(autostartStatusText({ ...base, state: "off" }), "Off.");
  assert.equal(
    autostartStatusText({ ...base, state: "pending" }),
    "Registers after you apply a layout.",
  );
  assert.equal(
    autostartStatusText({ ...base, state: "requiresApproval" }),
    "Waiting for your approval in System Settings > General > Login Items.",
  );
  assert.equal(
    autostartStatusText({ ...base, state: "disabledBySystem" }),
    "Turned off in Windows Settings > Apps > Startup.",
  );
  assert.equal(
    autostartStatusText({ ...base, state: "notFound" }),
    "MonHop moved since it was registered. Turn the switch off and on again.",
  );
  assert.equal(
    autostartStatusText({ ...base, state: "failed", message: "The OS refused the request." }),
    "The OS refused the request.",
  );
  assert.equal(
    autostartStatusText({ ...base, state: "failed", message: "" }),
    "Could not change how MonHop starts at login.",
  );
});

test("the open-settings button only shows when the OS needs the user", () => {
  assert.equal(showAutostartOpenSettings({ ...base, state: "requiresApproval" }), true);
  assert.equal(showAutostartOpenSettings({ ...base, state: "disabledBySystem" }), true);
  assert.equal(showAutostartOpenSettings({ ...base, state: "on" }), false);
  assert.equal(showAutostartOpenSettings({ ...base, state: "off" }), false);
  assert.equal(showAutostartOpenSettings({ ...base, state: "pending" }), false);
  assert.equal(showAutostartOpenSettings({ ...base, state: "notFound" }), false);
  assert.equal(showAutostartOpenSettings({ ...base, state: "failed" }), false);
});

test("the open-settings label and description follow the platform", () => {
  assert.equal(autostartOpenLabel("macos"), "Open Login Items");
  assert.equal(autostartOpenLabel("windows"), "Open Startup settings");
  assert.equal(
    autostartDescription("macos"),
    "MonHop starts in the menu bar at login and reconnects to this computer.",
  );
  assert.equal(
    autostartDescription("windows"),
    "MonHop starts in the system tray at login and reconnects to this computer.",
  );
});
