import { useEffect, useState } from 'react'

import type { DownloadKind } from './platform'
import { links } from './links'

/** Tauri names each asset by architecture; the suffix is the only stable part across versions. */
const assetSuffix: Record<DownloadKind, string> = {
  'mac-arm': '_aarch64.dmg',
  'mac-intel': '_x64.dmg',
  windows: '_x64-setup.exe',
}

type ReleaseAsset = { name: string; browser_download_url: string }
type Release = { tag_name: string; assets: ReleaseAsset[]; published_at?: string; html_url?: string }

export type LatestRelease = {
  version: string
  publishedAt: string | null
  notesUrl: string | null
  urls: Partial<Record<DownloadKind, string>>
}

let pending: Promise<LatestRelease | null> | null = null

async function request(): Promise<LatestRelease | null> {
  const response = await fetch(links.releasesApi, {
    headers: { Accept: 'application/vnd.github+json' },
  })
  // 404 is the expected answer until the first release is published.
  if (!response.ok) return null
  const release = (await response.json()) as Release
  const urls: Partial<Record<DownloadKind, string>> = {}
  for (const [kind, suffix] of Object.entries(assetSuffix) as [DownloadKind, string][]) {
    const asset = release.assets?.find((candidate) => candidate.name.endsWith(suffix))
    if (asset) urls[kind] = asset.browser_download_url
  }
  return {
    version: release.tag_name?.replace(/^v/, '') ?? '',
    publishedAt: release.published_at ?? null,
    notesUrl: release.html_url ?? null,
    urls,
  }
}

/** One request per page load; every caller shares the result and failures resolve to null. */
export function fetchLatestRelease(): Promise<LatestRelease | null> {
  pending ??= request().catch(() => null)
  return pending
}

export function downloadUrl(release: LatestRelease | null, kind: DownloadKind): string {
  return release?.urls[kind] ?? links.releasesPage
}

/** The releases API is the one network call the page makes, and only for the download path. */
export function useLatestRelease(): LatestRelease | null {
  const [release, setRelease] = useState<LatestRelease | null>(null)
  useEffect(() => {
    let live = true
    const load = async () => {
      const value = await fetchLatestRelease()
      if (live) setRelease(value)
    }
    void load()
    return () => {
      live = false
    }
  }, [])
  return release
}
