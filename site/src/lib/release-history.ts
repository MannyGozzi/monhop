import { useCallback, useEffect, useReducer } from 'react'

import { links } from './links.ts'

type ReleaseResponse = {
  tag_name?: unknown
  name?: unknown
  body?: unknown
  published_at?: unknown
  html_url?: unknown
  draft?: unknown
  prerelease?: unknown
}

type FetchResponse = {
  ok: boolean
  status: number
  json: () => Promise<unknown>
}

type Fetcher = (input: string, init: { headers: Record<string, string> }) => Promise<FetchResponse>

export type PublishedRelease = {
  tagName: string
  title: string | null
  body: string
  publishedAt: string
  htmlUrl: string
  anchor: string
}

export class ReleaseHistoryError extends Error {}

function releaseAnchor(tagName: string): string {
  return `release-${tagName.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/(^-|-$)/g, '')}`
}

function safeGithubUrl(value: unknown): string {
  if (typeof value !== 'string') throw new ReleaseHistoryError('A published release has no GitHub URL.')
  try {
    const url = new URL(value)
    if (url.protocol !== 'https:' || url.hostname !== 'github.com') throw new Error('untrusted URL')
    return url.toString()
  } catch {
    throw new ReleaseHistoryError('A published release has an unsafe GitHub URL.')
  }
}

function parseRelease(value: unknown): PublishedRelease | null {
  if (!value || typeof value !== 'object') throw new ReleaseHistoryError('GitHub returned an invalid release record.')
  const release = value as ReleaseResponse
  if (release.draft === true || release.prerelease === true) return null
  if (typeof release.tag_name !== 'string' || !release.tag_name.trim()) {
    throw new ReleaseHistoryError('A published release has no version tag.')
  }
  if (typeof release.published_at !== 'string' || Number.isNaN(Date.parse(release.published_at))) {
    throw new ReleaseHistoryError(`Published release ${release.tag_name} has an invalid date.`)
  }
  if (release.name !== undefined && release.name !== null && typeof release.name !== 'string') {
    throw new ReleaseHistoryError(`Published release ${release.tag_name} has an invalid title.`)
  }
  if (release.body !== undefined && release.body !== null && typeof release.body !== 'string') {
    throw new ReleaseHistoryError(`Published release ${release.tag_name} has invalid notes.`)
  }
  const tagName = release.tag_name.trim()
  return {
    tagName,
    title: typeof release.name === 'string' && release.name.trim() ? release.name.trim() : null,
    body: typeof release.body === 'string' ? release.body : '',
    publishedAt: release.published_at,
    htmlUrl: safeGithubUrl(release.html_url),
    anchor: releaseAnchor(tagName),
  }
}

export async function fetchReleaseHistory(fetcher: Fetcher = fetch): Promise<PublishedRelease[]> {
  const response = await fetcher(links.releaseHistoryApi, {
    headers: { Accept: 'application/vnd.github+json' },
  })
  if (!response.ok) throw new ReleaseHistoryError(`GitHub release history request failed (${response.status}).`)
  const payload = await response.json()
  if (!Array.isArray(payload)) throw new ReleaseHistoryError('GitHub returned an invalid release history.')
  return payload
    .map(parseRelease)
    .filter((release): release is PublishedRelease => release !== null)
    .toSorted((left, right) => Date.parse(right.publishedAt) - Date.parse(left.publishedAt))
}

export type ReleaseHistoryState =
  | { status: 'loading'; releases: PublishedRelease[] }
  | { status: 'empty'; releases: PublishedRelease[] }
  | { status: 'ready'; releases: PublishedRelease[] }
  | { status: 'error'; releases: PublishedRelease[]; error: ReleaseHistoryError }

type HistoryRequestState = ReleaseHistoryState & { requestId: number }

type HistoryAction =
  | { type: 'retry' }
  | { type: 'loaded'; requestId: number; releases: PublishedRelease[] }
  | { type: 'failed'; requestId: number; error: ReleaseHistoryError }

function historyReducer(state: HistoryRequestState, action: HistoryAction): HistoryRequestState {
  if (action.type === 'retry') return { status: 'loading', releases: [], requestId: state.requestId + 1 }
  if (action.requestId !== state.requestId) return state
  if (action.type === 'loaded') {
    return { status: action.releases.length ? 'ready' : 'empty', releases: action.releases, requestId: state.requestId }
  }
  return { status: 'error', releases: [], error: action.error, requestId: state.requestId }
}

export function useReleaseHistory(): ReleaseHistoryState & { retry: () => void } {
  const [state, dispatch] = useReducer(historyReducer, { status: 'loading', releases: [], requestId: 0 })
  const retry = useCallback(() => dispatch({ type: 'retry' }), [])

  useEffect(() => {
    let live = true
    const requestId = state.requestId
    void fetchReleaseHistory()
      .then((releases) => {
        if (live) dispatch({ type: 'loaded', requestId, releases })
        return undefined
      })
      .catch((error: unknown) => {
        if (live) {
          const message = error instanceof ReleaseHistoryError ? error : new ReleaseHistoryError('Release history is unavailable.')
          dispatch({ type: 'failed', requestId, error: message })
        }
        return undefined
      })
    return () => {
      live = false
    }
  }, [state.requestId])

  if (state.status === 'error') return { status: state.status, releases: state.releases, error: state.error, retry }
  return { status: state.status, releases: state.releases, retry }
}
