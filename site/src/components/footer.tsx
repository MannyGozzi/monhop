import { Heart } from 'lucide-react'

import { GithubMark } from '@/components/brand-marks'
import { links, product } from '@/lib/links'

export function Footer() {
  return (
    <footer id="footer" className="border-t border-border">
      <div className="mx-auto flex w-full max-w-6xl flex-col gap-6 px-5 py-10 sm:flex-row sm:items-center sm:justify-between sm:px-8">
        <div className="flex flex-col gap-1.5">
          <div className="flex items-center gap-2">
            <img src="/monhop.svg" alt="" width={24} height={24} className="size-6 rounded-[6px]" />
            <span className="font-semibold tracking-tight">{product.name}</span>
          </div>
          <p className="text-sm text-muted-foreground">Free software under {product.license}.</p>
        </div>

        <nav aria-label="Footer" className="flex flex-wrap items-center gap-x-5 gap-y-2 text-sm">
          <a
            href={links.source}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1.5 text-muted-foreground hover:text-foreground"
          >
            <GithubMark className="size-4" />
            GitHub
          </a>
          <a
            href={links.releasesPage}
            target="_blank"
            rel="noreferrer"
            className="text-muted-foreground hover:text-foreground"
          >
            Releases
          </a>
          <a
            href={links.sponsor}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1.5 text-muted-foreground hover:text-foreground"
          >
            <Heart aria-hidden className="size-4" />
            Support MonHop
          </a>
        </nav>
      </div>
    </footer>
  )
}
