// Whether asking a panel for `open` changes anything. A panel settled in that state, or already
// gliding toward it, is left alone, so a re-render never restarts or cuts its glide.
export function panelNeedsChange({ glidingTo = null, hidden, open, instant = false }) {
  if (glidingTo !== null) return glidingTo !== open || instant;
  return hidden === open;
}
