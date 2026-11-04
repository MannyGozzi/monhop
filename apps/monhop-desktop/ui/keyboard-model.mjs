// The keyboard switches as the Keyboard page shows them: each computer sets the one for its own
// keyboard, off unless Rust says on, and names where the other platform's keyboard sets its own.

import { switchContext } from "./switch-model.mjs";

export const CONTROL_AS_COMMAND_LABEL = "Use Ctrl as Command on a Mac";
export const CONTROL_AS_COMMAND_NOTE =
  "Ctrl+C, Ctrl+V and other shortcuts work on your Mac the way they do here. The Windows key becomes Control.";
export const COMMAND_AS_CONTROL_LABEL = "Use Command as Ctrl on Windows";
export const COMMAND_AS_CONTROL_NOTE =
  "Command+C, Command+V and other shortcuts work on your PC the way they do here. Control becomes the Windows key.";

// Per platform: the page's description, this keyboard's switch and its Rust commands, and the row
// for the other platform's keyboard, whose switch lives on that computer.
const KEYBOARDS = {
  windows: {
    description: "When this PC's keyboard controls a Mac, or a Mac's keyboard controls this PC.",
    label: CONTROL_AS_COMMAND_LABEL,
    note: CONTROL_AS_COMMAND_NOTE,
    commands: { status: "control_as_command_status", set: "control_as_command_set_enabled" },
    other: {
      title: "Command as Ctrl on Windows",
      detail:
        "For a Mac keyboard that controls this PC. Change it in MonHop on that Mac, under Settings › Keyboard.",
    },
  },
  macos: {
    description: "When this Mac's keyboard controls a PC, or a PC's keyboard controls this Mac.",
    label: COMMAND_AS_CONTROL_LABEL,
    note: COMMAND_AS_CONTROL_NOTE,
    commands: { status: "command_as_control_status", set: "command_as_control_set_enabled" },
    other: {
      title: "Ctrl as Command on a Mac",
      detail:
        "For a Windows keyboard that controls this Mac. Change it in MonHop on that PC, under Settings › Keyboard.",
    },
  },
};

// The Rust commands for this computer's own switch; another platform has none.
export function keyboardCommands(platform) {
  return KEYBOARDS[platform]?.commands;
}

// The page's context: a Mac or a PC only, and only once Rust has answered, so an older backend
// leaves the page out.
export function keyboardContext(platform, view, pending) {
  const keyboard = KEYBOARDS[platform];
  if (!keyboard) return undefined;
  const own = switchContext(platform, platform, view, pending, false);
  if (!own) return undefined;
  const { description, label, note, other } = keyboard;
  return { ...own, description, label, note, other };
}
