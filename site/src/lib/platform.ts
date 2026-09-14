export type Platform = 'macos' | 'windows' | 'other'

/** Release asset kinds, one per file Tauri publishes. */
export type DownloadKind = 'mac-arm' | 'mac-intel' | 'windows'

type NavigatorUAData = { platform?: string }

/** userAgentData is the only hint Chromium still keeps accurate; the legacy fields are the fallback. */
export function detectPlatform(): Platform {
  if (typeof navigator === 'undefined') return 'other'
  const ua = navigator as Navigator & { userAgentData?: NavigatorUAData }
  const hint = `${ua.userAgentData?.platform ?? ''} ${navigator.platform ?? ''} ${navigator.userAgent ?? ''}`.toLowerCase()
  if (hint.includes('mac')) return 'macos'
  if (hint.includes('win')) return 'windows'
  return 'other'
}

export const platformLabel: Record<Platform, string> = {
  macos: 'macOS',
  windows: 'Windows',
  other: 'your computer',
}
