import assert from 'node:assert/strict'
import test from 'node:test'

import { fetchReleaseHistory, ReleaseHistoryError } from './release-history.ts'

function response(payload: unknown, ok = true, status = 200) {
  return async () => ({ ok, status, json: async () => payload })
}

test('reads published releases newest first and excludes drafts and prereleases', async () => {
  let request: { input: string; accept: string } | null = null
  const releases = await fetchReleaseHistory(async (input, init) => {
    request = { input, accept: init.headers.Accept }
    return {
      ok: true,
      status: 200,
      json: async () => [
        {
          tag_name: 'v0.1.0',
          name: 'First public release',
          body: '### Added\n- A real-looking local fixture only.',
          published_at: '2026-09-10T00:00:00Z',
          html_url: 'https://github.com/MannyGozzi/monhop/releases/tag/v0.1.0',
          draft: false,
          prerelease: false,
        },
        {
          tag_name: 'v0.2.0-draft',
          published_at: '2026-09-11T00:00:00Z',
          html_url: 'https://github.com/MannyGozzi/monhop/releases/tag/v0.2.0-draft',
          draft: true,
        },
        {
          tag_name: 'v0.1.1-rc',
          published_at: '2026-09-12T00:00:00Z',
          html_url: 'https://github.com/MannyGozzi/monhop/releases/tag/v0.1.1-rc',
          prerelease: true,
        },
        {
          tag_name: 'v0.0.9',
          body: null,
          published_at: '2026-09-01T00:00:00Z',
          html_url: 'https://github.com/MannyGozzi/monhop/releases/tag/v0.0.9',
          draft: false,
          prerelease: false,
        },
      ],
    }
  })

  assert.deepEqual(releases.map((release) => release.tagName), ['v0.1.0', 'v0.0.9'])
  assert.equal(releases[0].anchor, 'release-v0-1-0')
  assert.equal(releases[1].body, '')
  assert.deepEqual(request, {
    input: 'https://api.github.com/repos/MannyGozzi/monhop/releases?per_page=100',
    accept: 'application/vnd.github+json',
  })
})

test('distinguishes failed and invalid GitHub responses from an empty release list', async () => {
  await assert.rejects(fetchReleaseHistory(response([], false, 404)), ReleaseHistoryError)
  await assert.rejects(fetchReleaseHistory(response({ releases: [] })), ReleaseHistoryError)
  await assert.rejects(fetchReleaseHistory(response([
    {
      tag_name: 'v0.1.0',
      published_at: '2026-09-10T00:00:00Z',
      html_url: 'javascript:alert(1)',
    },
  ])), ReleaseHistoryError)
  assert.deepEqual(await fetchReleaseHistory(response([])), [])
})
