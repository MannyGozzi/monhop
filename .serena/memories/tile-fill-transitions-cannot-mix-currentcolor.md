# Tile fill transitions cannot mix currentColor

Chromium (so WebView2, the Windows app) cannot interpolate a `fill` whose value is
`color-mix(..., currentColor ...)`. For the whole transition it reports and paints the fill as
`rgba(0, 0, 0, 0)` and then snaps to the end value, so a display tile flashes dark for
`--motion-slow` whenever its hover tone changes. Both ends of the transition count: a resting
fill that mixes currentColor breaks it as well.

The tile hue therefore lives in `--tile-hue` on `.arrangement-group` (set by the `is-*` and
`data-tone` rules in `ui/styles.css`), with `color: var(--tile-hue)` kept for strokes and
glyphs. `.arrangement-display-screen` fills mix `var(--tile-hue)`, never currentColor. Found
2026-10-22 by sampling `getComputedStyle(screen).fill` every 40 ms during a hover in the
preview harness.

Related: `--check-ui` cannot pass while the Mac screen is locked (WebKit stops drawing, so it
ends with "render deadline at render at width 1180" even on a good build). Check
`CGSSessionScreenIsLocked` in `CGSessionCopyCurrentDictionary()` before reading that failure as
a regression.
