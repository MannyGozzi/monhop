import { useState } from 'react'
import { Download } from 'lucide-react'

import { AppleMark, WindowsMark } from '@/components/brand-marks'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'
import { detectPlatform, type DownloadKind } from '@/lib/platform'
import { downloadUrl, useLatestRelease } from '@/lib/releases'

function PrimaryLink({
  href,
  children,
  className,
}: {
  href: string
  children: React.ReactNode
  className?: string
}) {
  return (
    <Button
      asChild
      size="lg"
      className={cn(
        'h-12 rounded-full px-6 text-[0.95rem] shadow-[0_10px_30px_-12px_var(--primary)]',
        className,
      )}
    >
      <a href={href}>
        <Download aria-hidden className="size-4" />
        {children}
      </a>
    </Button>
  )
}

export function DownloadButton({ className }: { className?: string }) {
  const [platform] = useState(detectPlatform)
  const release = useLatestRelease()

  const link = (kind: DownloadKind) => downloadUrl(release, kind)
  const versionNote = release?.version ? `Version ${release.version}` : 'Latest release'

  return (
    <div className={cn('flex flex-col items-center gap-3', className)}>
      <div className="flex flex-col items-center gap-3 sm:flex-row">
        {platform === 'windows' ? (
          <PrimaryLink href={link('windows')}>Download for Windows</PrimaryLink>
        ) : (
          <PrimaryLink href={link('mac-arm')}>Download for macOS</PrimaryLink>
        )}
        {platform === 'other' ? (
          <PrimaryLink href={link('windows')} className="sm:ml-1">
            Download for Windows
          </PrimaryLink>
        ) : null}
      </div>

      <p className="flex flex-wrap items-center justify-center gap-x-2 gap-y-1 text-sm text-muted-foreground">
        <span>{versionNote}</span>
        <span aria-hidden>·</span>
        {platform === 'windows' ? (
          <a
            href={link('mac-arm')}
            className="inline-flex items-center gap-1.5 underline underline-offset-4 hover:text-foreground"
          >
            <AppleMark className="size-3.5" />
            Also for macOS
          </a>
        ) : null}
        {platform === 'macos' ? (
          <a
            href={link('windows')}
            className="inline-flex items-center gap-1.5 underline underline-offset-4 hover:text-foreground"
          >
            <WindowsMark className="size-3.5" />
            Also for Windows
          </a>
        ) : null}
        {platform === 'other' ? <span>Free, no account</span> : null}
      </p>
    </div>
  )
}
