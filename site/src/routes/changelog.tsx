import { createFileRoute } from '@tanstack/react-router'
import { ArrowUpRight, FileText, RefreshCw } from 'lucide-react'
import { useEffect, useRef } from 'react'

import { InlineContent, ReleaseNotes } from '@/components/release-notes'
import { Button } from '@/components/ui/button'
import { links, product } from '@/lib/links'
import { displayReleaseSections, releaseSummary } from '@/lib/release-markdown'
import { useReleaseHistory } from '@/lib/release-history'

function releaseDate(value: string): string {
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'long' }).format(new Date(value))
}

function titleIncludesVersion(title: string, tagName: string): boolean {
  const normalizedTitle = title.toLowerCase()
  return normalizedTitle.includes(tagName.toLowerCase()) || normalizedTitle.includes(tagName.replace(/^v/, '').toLowerCase())
}

function initialReleaseFragment(): string | null {
  if (typeof window === 'undefined') return null
  const fragment = window.location.hash.slice(1)
  return fragment.startsWith('release-') ? fragment : null
}

function LoadingState() {
  return (
    <div aria-live="polite" className="glass rounded-2xl p-6 sm:p-8">
      <p className="text-sm font-medium">Loading published release notes…</p>
      <div className="mt-5 space-y-3" aria-hidden>
        <div className="h-3 w-24 animate-pulse rounded-full bg-muted" />
        <div className="h-6 w-48 animate-pulse rounded-full bg-muted" />
        <div className="h-3 w-full animate-pulse rounded-full bg-muted" />
        <div className="h-3 w-4/5 animate-pulse rounded-full bg-muted" />
      </div>
    </div>
  )
}

function EmptyState() {
  return (
    <div className="glass rounded-2xl p-6 sm:p-8">
      <FileText aria-hidden className="size-5 text-primary" />
      <h2 className="mt-4 text-xl font-semibold tracking-tight">No published releases yet</h2>
      <p className="mt-2 max-w-lg text-sm/6 text-muted-foreground">
        When MonHop has a public release, its notes will appear here with the installers.
      </p>
    </div>
  )
}

function ErrorState({ retry }: { retry: () => void }) {
  return (
    <div className="glass rounded-2xl p-6 sm:p-8">
      <h2 className="text-xl font-semibold tracking-tight">Release history is unavailable</h2>
      <p className="mt-2 max-w-lg text-sm/6 text-muted-foreground">
        GitHub did not return published notes. Try again or check the release page directly.
      </p>
      <div className="mt-5 flex flex-wrap gap-2">
        <Button type="button" variant="outline" className="rounded-full" onClick={retry}>
          <RefreshCw aria-hidden className="size-4" />
          Try again
        </Button>
        <Button asChild variant="ghost" className="rounded-full">
          <a href={links.releasesPage} target="_blank" rel="noreferrer">
            GitHub releases
            <ArrowUpRight aria-hidden className="size-4" />
          </a>
        </Button>
      </div>
    </div>
  )
}

function ChangelogPage() {
  const history = useReleaseHistory()
  const fragment = useRef(initialReleaseFragment())
  const fragmentHandled = useRef(false)

  useEffect(() => {
    if (fragmentHandled.current || history.status !== 'ready' || !fragment.current) return
    const target = document.getElementById(fragment.current)
    if (!target) return
    fragmentHandled.current = true
    target.scrollIntoView({ behavior: 'auto', block: 'start' })
  }, [history.status])

  return (
    <section className="dot-grid min-h-dvh px-5 pt-32 pb-20 sm:px-8 sm:pt-40 sm:pb-28">
      <div className="mx-auto w-full max-w-4xl">
        <header className="max-w-2xl">
          <p className="text-xs font-semibold tracking-[0.16em] text-primary uppercase">Release notes</p>
          <h1 className="mt-4 text-balance text-4xl font-semibold tracking-tight sm:text-5xl">What’s new in MonHop</h1>
          <p className="mt-5 text-pretty text-base/7 text-muted-foreground">
            New features, improvements, and fixes, published directly from GitHub.
          </p>
        </header>

        <div className="mt-12">
          {history.status === 'loading' ? <LoadingState /> : null}
          {history.status === 'empty' ? <EmptyState /> : null}
          {history.status === 'error' ? <ErrorState retry={history.retry} /> : null}
          {history.status === 'ready' ? (
            <ol className="relative space-y-6 before:absolute before:top-4 before:bottom-4 before:left-[7px] before:w-px before:bg-border sm:space-y-8">
              {history.releases.map((release) => {
                const summary = releaseSummary(release.body)
                const details = displayReleaseSections(release.body, summary !== null)
                const title = release.title ?? `${product.name} ${release.tagName}`
                return (
                  <li key={release.tagName} className="relative pl-8 sm:pl-10">
                    <span
                      aria-hidden
                      className="absolute top-7 left-0 size-[15px] rounded-full border-4 border-background bg-primary shadow-[0_0_0_1px_var(--border)]"
                    />
                    <article id={release.anchor} className="glass scroll-mt-28 rounded-2xl p-6 sm:p-8">
                      <div className="flex flex-wrap items-start justify-between gap-4">
                        <div>
                          <time dateTime={release.publishedAt} className="text-xs font-medium tracking-wide text-muted-foreground">
                            {releaseDate(release.publishedAt)}
                          </time>
                          <h2 className="mt-2 text-2xl font-semibold tracking-tight">
                            <a href={`#${release.anchor}`} className="rounded-sm hover:text-primary focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none">
                              {title}
                            </a>
                          </h2>
                          {!titleIncludesVersion(title, release.tagName) ? (
                            <a
                              href={`#${release.anchor}`}
                              className="mt-2 inline-flex rounded-sm text-xs font-semibold tracking-[0.14em] text-primary uppercase focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none"
                            >
                              Version {release.tagName}
                            </a>
                          ) : null}
                        </div>
                        <a
                          href={release.htmlUrl}
                          target="_blank"
                          rel="noreferrer"
                          className="inline-flex items-center gap-1 text-sm text-muted-foreground underline underline-offset-4 hover:text-foreground"
                        >
                          On GitHub
                          <ArrowUpRight aria-hidden className="size-3.5" />
                        </a>
                      </div>
                      {summary ? (
                        <p className="mt-5 max-w-2xl text-pretty text-base/7 text-foreground/85">
                          <InlineContent nodes={summary} />
                        </p>
                      ) : null}
                      {details.length || !release.body.trim() ? (
                        <div className="mt-7 border-t border-border pt-6">
                          <ReleaseNotes body={release.body} omitLead={summary !== null} />
                        </div>
                      ) : null}
                    </article>
                  </li>
                )
              })}
            </ol>
          ) : null}
        </div>
      </div>
    </section>
  )
}

export const Route = createFileRoute('/changelog')({
  component: ChangelogPage,
  head: () => ({ meta: [{ title: 'Release notes — MonHop' }] }),
})
