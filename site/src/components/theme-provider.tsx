import { useCallback, useEffect, useMemo, useState } from 'react'

import {
  ThemeContext,
  themeStorageKey,
  type ResolvedTheme,
  type ThemePreference,
} from '@/lib/theme'

const order: ThemePreference[] = ['system', 'light', 'dark']

function readStored(): ThemePreference {
  try {
    const stored = localStorage.getItem(themeStorageKey)
    if (stored === 'light' || stored === 'dark' || stored === 'system') return stored
  } catch {
    // Private-mode storage denial falls back to the system preference.
  }
  return 'system'
}

function systemTheme(): ResolvedTheme {
  return globalThis.matchMedia?.('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'
}

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const [preference, setPreferenceState] = useState<ThemePreference>(readStored)
  const [systemResolved, setSystemResolved] = useState<ResolvedTheme>(systemTheme)

  useEffect(() => {
    const query = globalThis.matchMedia('(prefers-color-scheme: dark)')
    const onChange = () => setSystemResolved(query.matches ? 'dark' : 'light')
    query.addEventListener('change', onChange)
    return () => query.removeEventListener('change', onChange)
  }, [])

  const resolved = preference === 'system' ? systemResolved : preference

  useEffect(() => {
    document.documentElement.classList.toggle('dark', resolved === 'dark')
    document.documentElement.dataset.theme = resolved
  }, [resolved])

  const setPreference = useCallback((next: ThemePreference) => {
    setPreferenceState(next)
    try {
      localStorage.setItem(themeStorageKey, next)
    } catch {
      // Preference stays for this page only when storage is unavailable.
    }
  }, [])

  const value = useMemo(
    () => ({
      preference,
      resolved,
      setPreference,
      cycle: () => setPreference(order[(order.indexOf(preference) + 1) % order.length]),
    }),
    [preference, resolved, setPreference],
  )

  return <ThemeContext value={value}>{children}</ThemeContext>
}
