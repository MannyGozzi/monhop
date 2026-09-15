import { Download as DownloadIcon } from 'lucide-react'

import { AppleMark, WindowsMark } from '@/components/brand-marks'
import { Reveal } from '@/components/reveal'
import { SectionHeading } from '@/components/section-heading'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Separator } from '@/components/ui/separator'
import { product } from '@/lib/links'
import type { DownloadKind } from '@/lib/platform'
import { downloadUrl, useLatestRelease } from '@/lib/releases'

const platforms = [
  {
    id: 'mac',
    mark: AppleMark,
    title: 'macOS',
    requirement: product.macRequirement,
    note: 'Until MonHop is notarized, macOS asks you to allow it once in System Settings > Privacy & Security.',
    builds: [
      { kind: 'mac-arm' as DownloadKind, label: 'Apple silicon' },
      { kind: 'mac-intel' as DownloadKind, label: 'Intel' },
    ],
  },
  {
    id: 'windows',
    mark: WindowsMark,
    title: 'Windows',
    requirement: product.windowsRequirement,
    note: 'Windows SmartScreen may ask you to confirm the installer.',
    builds: [{ kind: 'windows' as DownloadKind, label: 'Installer' }],
  },
]

/** The viewer's locale and calendar, so the date reads naturally wherever the page is opened. */
function releaseDate(iso: string): string {
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'long' }).format(new Date(iso))
}

export function Download() {
  const release = useLatestRelease()

  return (
    <section id="download" className="dot-grid relative px-5 py-20 sm:px-8 sm:py-28">
      <div className="mx-auto w-full max-w-6xl">
        <Reveal>
          <SectionHeading
            title="Install it on every computer"
            description="Free software, no account, no sign-up. Install MonHop on each computer you want to reach and pair them once."
          />
        </Reveal>

        {release ? (
          <Reveal delay={0.05}>
            <p className="mt-6 flex flex-wrap items-center justify-center gap-x-2 gap-y-1 text-sm text-muted-foreground">
              <span>Version {release.version}</span>
              {release.publishedAt ? (
                <>
                  <span aria-hidden>·</span>
                  <span>Released {releaseDate(release.publishedAt)}</span>
                </>
              ) : null}
              <>
                <span aria-hidden>·</span>
                <a href="/changelog" className="underline underline-offset-4 hover:text-foreground">
                  Release notes
                </a>
              </>
            </p>
          </Reveal>
        ) : null}

        <div className="mx-auto mt-12 grid max-w-4xl gap-4 sm:mt-16 sm:grid-cols-2">
          {platforms.map((platform, index) => {
            const Mark = platform.mark
            return (
              <Reveal key={platform.id} delay={index * 0.07}>
                <div className="glass flex h-full flex-col gap-4 rounded-2xl p-6">
                  <div className="flex items-center gap-3">
                    <span className="flex size-10 items-center justify-center rounded-xl border border-border bg-background/70">
                      <Mark className="size-5" />
                    </span>
                    <div className="flex flex-col">
                      <h3 className="text-lg font-semibold tracking-tight">{platform.title}</h3>
                      <p className="text-sm text-muted-foreground">{platform.requirement}</p>
                    </div>
                    {release?.version ? (
                      <Badge variant="secondary" className="ml-auto">
                        {release.version}
                      </Badge>
                    ) : null}
                  </div>

                  <div className="flex flex-wrap gap-2">
                    {platform.builds.map((build) => (
                      <Button asChild key={build.kind} variant="outline" className="rounded-full">
                        <a href={downloadUrl(release, build.kind)}>
                          <DownloadIcon aria-hidden className="size-4" />
                          {build.label}
                        </a>
                      </Button>
                    ))}
                  </div>

                  <Separator className="mt-auto" />
                  <p className="text-sm/6 text-muted-foreground">{platform.note}</p>
                </div>
              </Reveal>
            )
          })}
        </div>

        <Reveal delay={0.14}>
          <p className="mt-8 text-center text-sm text-muted-foreground">
            Free software under {product.license}. Donations keep it going, nothing is ever gated.
          </p>
        </Reveal>
      </div>
    </section>
  )
}
