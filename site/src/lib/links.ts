/** One repository holds the source, the releases and the updater manifest. */
const repo = 'MannyGozzi/monhop'

export const links = {
  source: `https://github.com/${repo}`,
  releasesPage: `https://github.com/${repo}/releases`,
  releasesApi: `https://api.github.com/repos/${repo}/releases/latest`,
  releaseHistoryApi: `https://api.github.com/repos/${repo}/releases?per_page=100`,
  sponsor: 'https://github.com/sponsors/MannyGozzi',
  site: 'https://www.monhop.com',
} as const

export const product = {
  name: 'MonHop',
  license: 'GPL-3.0-or-later',
  macRequirement: 'macOS 14 or later',
  windowsRequirement: 'Windows 10 or 11, 64-bit',
} as const
