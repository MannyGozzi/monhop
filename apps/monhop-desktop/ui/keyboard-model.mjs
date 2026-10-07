// Ctrl as Command as the Home card shows it: a Windows-only switch that matters when this keyboard
// controls a Mac, off unless Rust says on.

import { switchContext } from "./switch-model.mjs";

export const CONTROL_AS_COMMAND_LABEL = "Use Ctrl as Command on a Mac";
export const CONTROL_AS_COMMAND_NOTE =
  "Ctrl+C, Ctrl+V and other shortcuts work on your Mac the way they do here. The Windows key becomes Control.";

export function controlAsCommandContext(platform, view, pending) {
  return switchContext(platform, "windows", view, pending, false);
}
