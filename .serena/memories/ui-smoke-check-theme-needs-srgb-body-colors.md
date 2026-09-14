# --check-ui theme flag needs sRGB body colors

The headless UI smoke check (`apps/monhop-desktop/src/ui_smoke.rs`, RENDER_CHECK) reads
`getComputedStyle(document.body).color` and `.backgroundColor` and extracts the first three
integers with `/\d+/g`, expecting `rgb(r, g, b)`. WebKit serializes `oklch()` computed colors
as `oklch(...)`, so `--background`/`--foreground` on `:root` and the dark block must stay hex or
rgb even when every other token is oklch. Symptom: `theme=false` in the UI categories line with
everything else true. Fixed 2026-09-11 by keeping those two tokens in hex in `ui/styles.css`
and `ui/trial.css`.

Related: the check also requires `.window-heading` left edge >= 82px on macOS (header padding
for traffic lights), body font <= 14px, headings <= 30px, and buttons 28-40px tall.
