/* oxlint-disable react/no-array-index-key -- Parsed notes are read-only and may contain identical words, bullets or sections. */
import type { InlineNode, MarkdownBlock, ReleaseSection } from '@/lib/release-markdown'
import { displayReleaseSections } from '@/lib/release-markdown'

export function InlineContent({ nodes }: { nodes: InlineNode[] }) {
  return (
    <>
      {nodes.map((node, index) => {
        const key = `${node.kind}-${index}`
        if (node.kind === 'strong') return <strong key={key}>{node.value}</strong>
        if (node.kind === 'emphasis') return <em key={key}>{node.value}</em>
        if (node.kind === 'link') {
          return (
            <a
              key={key}
              href={node.href}
              target="_blank"
              rel="noreferrer"
              className="underline underline-offset-4 hover:text-foreground"
            >
              {node.value}
            </a>
          )
        }
        return <span key={key}>{node.value}</span>
      })}
    </>
  )
}

function NoteBlock({ block }: { block: MarkdownBlock }) {
  if (block.kind === 'paragraph') {
    return (
      <p className="text-pretty text-sm/6 text-muted-foreground">
        <InlineContent nodes={block.content} />
      </p>
    )
  }
  const List = block.ordered ? 'ol' : 'ul'
  const className = block.ordered
    ? 'list-decimal space-y-2 pl-5 text-sm/6 text-muted-foreground marker:text-foreground/55'
    : 'space-y-2 text-sm/6 text-muted-foreground'

  return (
    <List className={className}>
      {block.items.map((item, index) => (
        <li
          key={index}
          className={block.ordered ? '' : 'relative pl-4 before:absolute before:top-[0.72em] before:left-0 before:size-1 before:rounded-full before:bg-primary/70'}
        >
          <InlineContent nodes={item} />
        </li>
      ))}
    </List>
  )
}

function NoteSection({ section }: { section: ReleaseSection }) {
  return (
    <section className="space-y-3">
      {section.title ? (
        <h3 className="text-xs font-semibold tracking-[0.14em] text-primary uppercase">{section.label}</h3>
      ) : null}
      <div className="space-y-3">
        {section.blocks.map((block, index) => <NoteBlock key={index} block={block} />)}
      </div>
    </section>
  )
}

export function ReleaseNotes({ body, omitLead = false }: { body: string; omitLead?: boolean }) {
  const sections = displayReleaseSections(body, omitLead)
  if (!sections.length && omitLead) return null
  if (!sections.length) return <p className="text-sm/6 text-muted-foreground">This release was published without written notes.</p>
  return <div className="space-y-7">{sections.map((section, index) => <NoteSection key={index} section={section} />)}</div>
}
