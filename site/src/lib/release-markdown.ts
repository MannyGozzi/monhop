export type InlineNode =
  | { kind: 'text'; value: string }
  | { kind: 'strong'; value: string }
  | { kind: 'emphasis'; value: string }
  | { kind: 'link'; value: string; href: string }

export type MarkdownBlock =
  | { kind: 'paragraph'; content: InlineNode[] }
  | { kind: 'list'; ordered: boolean; items: InlineNode[][] }

export type ReleaseSection = {
  title: string | null
  label: string | null
  blocks: MarkdownBlock[]
}

const labels: Record<string, string> = {
  added: 'New',
  changed: 'Improved',
  fixed: 'Fixed',
  security: 'Security',
}

const heading = /^\s{0,3}#{1,6}\s+(.+?)\s*#*\s*$/
const listItem = /^\s*(?:([-+*])|(\d+)\.)\s+(.+)$/
const inline = /\[([^\]]+)\]\(([^\s)]+)\)|(\*\*|__)(.+?)\3|(\*|_)(.+?)\5/g

function safeLink(value: string): string | null {
  try {
    const url = new URL(value)
    return url.protocol === 'https:' ? url.toString() : null
  } catch {
    return null
  }
}

function sectionLabel(title: string): string {
  const key = title.trim().toLowerCase()
  return Object.hasOwn(labels, key) ? labels[key] : title
}

export function parseInline(value: string): InlineNode[] {
  const nodes: InlineNode[] = []
  let index = 0
  for (const match of value.matchAll(inline)) {
    const start = match.index ?? 0
    if (start > index) nodes.push({ kind: 'text', value: value.slice(index, start) })
    if (match[1] !== undefined) {
      const href = safeLink(match[2])
      nodes.push(href ? { kind: 'link', value: match[1], href } : { kind: 'text', value: match[0] })
    } else if (match[3] !== undefined) {
      nodes.push({ kind: 'strong', value: match[4] })
    } else {
      nodes.push({ kind: 'emphasis', value: match[6] })
    }
    index = start + match[0].length
  }
  if (index < value.length) nodes.push({ kind: 'text', value: value.slice(index) })
  return nodes.length ? nodes : [{ kind: 'text', value }]
}

/** Keep unsupported Markdown as text instead of interpreting it as HTML. */
export function parseReleaseMarkdown(markdown: string): ReleaseSection[] {
  const sections: ReleaseSection[] = []
  let current: ReleaseSection = { title: null, label: null, blocks: [] }
  let paragraph: string[] = []
  let list: string[] = []
  let ordered = false

  const flushParagraph = () => {
    if (!paragraph.length) return
    current.blocks.push({ kind: 'paragraph', content: parseInline(paragraph.join(' ')) })
    paragraph = []
  }
  const flushList = () => {
    if (!list.length) return
    current.blocks.push({ kind: 'list', ordered, items: list.map(parseInline) })
    list = []
  }
  const flushBlocks = () => {
    flushParagraph()
    flushList()
  }
  const pushCurrent = () => {
    flushBlocks()
    if (current.title || current.blocks.length) sections.push(current)
  }

  for (const line of markdown.replace(/\r\n?/g, '\n').split('\n')) {
    const title = line.match(heading)
    if (title) {
      pushCurrent()
      current = { title: title[1], label: sectionLabel(title[1]), blocks: [] }
      continue
    }
    const item = line.match(listItem)
    if (item) {
      flushParagraph()
      const itemOrdered = item[2] !== undefined
      if (list.length && itemOrdered !== ordered) flushList()
      ordered = itemOrdered
      list.push(item[3])
      continue
    }
    if (!line.trim()) {
      flushBlocks()
      continue
    }
    flushList()
    paragraph.push(line.trim())
  }
  pushCurrent()
  return sections
}

/** The lead paragraph becomes the timeline summary and is not rendered twice. */
export function releaseSummary(markdown: string): InlineNode[] | null {
  const lead = parseReleaseMarkdown(markdown)[0]
  const paragraph = lead?.title === null && lead.blocks[0]?.kind === 'paragraph' ? lead.blocks[0] : null
  if (!paragraph || paragraph.kind !== 'paragraph') return null
  return paragraph.content.length ? paragraph.content : null
}

/** Omit only the paragraph already shown as the card summary; all other lead content remains. */
export function displayReleaseSections(markdown: string, omitSummary = false): ReleaseSection[] {
  const sections = parseReleaseMarkdown(markdown)
  const lead = sections[0]
  if (!omitSummary || lead?.title !== null) return sections
  if (lead.blocks[0]?.kind !== 'paragraph') return sections

  const remainingLead = { ...lead, blocks: lead.blocks.slice(1) }
  return [...(remainingLead.blocks.length ? [remainingLead] : []), ...sections.slice(1)]
}
