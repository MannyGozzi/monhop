import assert from 'node:assert/strict'
import test from 'node:test'

import { displayReleaseSections, parseInline, parseReleaseMarkdown, releaseSummary } from './release-markdown.ts'

test('groups headings into semantic sections and keeps meaningful Markdown text', () => {
  const sections = parseReleaseMarkdown(`A concise introduction.\n\n### Added\n- **Quick setup** for new computers\n- [Guide](https://example.com/guide)\n\n### Fixed\n1. Restored a missing option`)

  assert.equal(sections.length, 3)
  assert.equal(sections[0].title, null)
  assert.equal(sections[1].label, 'New')
  assert.equal(sections[2].label, 'Fixed')
  assert.equal(sections[2].blocks[0].kind, 'list')
  assert.deepEqual(releaseSummary('A short intro.\n\n### Changed\n- Made pairing clearer'), [
    { kind: 'text', value: 'A short intro.' },
  ])
})

test('renders untrusted HTML and unsafe URLs as readable text rather than links', () => {
  const nodes = parseInline('Read <script>bad()</script> and [this](javascript:alert).')

  assert.deepEqual(nodes, [
    { kind: 'text', value: 'Read <script>bad()</script> and ' },
    { kind: 'text', value: '[this](javascript:alert)' },
    { kind: 'text', value: '.' },
  ])
})

test('keeps safe formatting in the lead summary and leaves unsafe links as text', () => {
  const summary = releaseSummary('Read the [upgrade guide](https://example.com/upgrade) with **care** *today* and [this](javascript:alert).')

  assert.deepEqual(summary, [
    { kind: 'text', value: 'Read the ' },
    { kind: 'link', value: 'upgrade guide', href: 'https://example.com/upgrade' },
    { kind: 'text', value: ' with ' },
    { kind: 'strong', value: 'care' },
    { kind: 'text', value: ' ' },
    { kind: 'emphasis', value: 'today' },
    { kind: 'text', value: ' and ' },
    { kind: 'text', value: '[this](javascript:alert)' },
    { kind: 'text', value: '.' },
  ])
})

test('keeps prototype-named headings as literal labels', () => {
  for (const title of ['__proto__', 'constructor', 'toString']) {
    const section = parseReleaseMarkdown(`### ${title}\n- Kept literally.`)[0]
    assert.equal(section.label, title)
  }
})

test('keeps unsupported Markdown and every lead block visible', () => {
  const unsupported = '> A quoted line with `code` stays readable.'
  assert.equal(parseReleaseMarkdown(unsupported)[0].blocks[0].kind, 'paragraph')
  const markdown = 'First paragraph.\n\nSecond paragraph.\n\n- A lead bullet.\n\n### Fixed\n- A fix.'
  assert.deepEqual(releaseSummary(markdown), [{ kind: 'text', value: 'First paragraph.' }])
  assert.deepEqual(displayReleaseSections(markdown, true), [
    {
      title: null,
      label: null,
      blocks: [
        { kind: 'paragraph', content: [{ kind: 'text', value: 'Second paragraph.' }] },
        { kind: 'list', ordered: false, items: [[{ kind: 'text', value: 'A lead bullet.' }]] },
      ],
    },
    {
      title: 'Fixed',
      label: 'Fixed',
      blocks: [{ kind: 'list', ordered: false, items: [[{ kind: 'text', value: 'A fix.' }]] }],
    },
  ])

  const plainLead = 'A complete lead paragraph.\n\n- A lead bullet without a heading.'
  assert.deepEqual(releaseSummary(plainLead), [{ kind: 'text', value: 'A complete lead paragraph.' }])
  assert.deepEqual(displayReleaseSections(plainLead, true), [
    {
      title: null,
      label: null,
      blocks: [{ kind: 'list', ordered: false, items: [[{ kind: 'text', value: 'A lead bullet without a heading.' }]] }],
    },
  ])
})
