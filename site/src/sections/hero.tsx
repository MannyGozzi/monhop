import { HeroDemo } from '@/components/demo'
import { GithubMark } from '@/components/brand-marks'
import { DownloadButton } from '@/components/download-button'
import { Reveal } from '@/components/reveal'
import { Button } from '@/components/ui/button'
import { links } from '@/lib/links'

export function Hero() {
  return (
    <section id="hero" className="relative overflow-hidden px-5 pt-28 pb-16 sm:px-8 sm:pt-36 sm:pb-24">
      <div
        aria-hidden
        className="pointer-events-none absolute inset-x-0 top-0 h-[42rem]"
        style={{
          background:
            'radial-gradient(46rem 26rem at 50% 0%, color-mix(in oklab, var(--glow) 22%, transparent), transparent 68%), radial-gradient(30rem 20rem at 50% 14%, color-mix(in oklab, var(--primary) 14%, transparent), transparent 70%)',
        }}
      />

      <div className="relative mx-auto flex w-full max-w-6xl flex-col items-center">
        <Reveal className="flex flex-col items-center gap-5 text-center">
          <h1 className="max-w-4xl text-balance text-[2.25rem]/[1.08] font-semibold tracking-[-0.03em] sm:text-6xl/[1.05] lg:text-[4.5rem]/[1.03]">
            One keyboard.{' '}
            <br className="hidden sm:block" />
            All your computers.
          </h1>
          <p className="max-w-2xl text-pretty text-base/7 text-muted-foreground sm:text-lg/8">
            Move your pointer across the edge of the screen and your keyboard follows. Pair every
            Mac and Windows PC on your desk, in any combination, entirely on your own network.
          </p>
        </Reveal>

        <Reveal delay={0.08} className="mt-9 flex flex-col items-center gap-4">
          <DownloadButton />
          <Button asChild variant="ghost" size="sm" className="rounded-full text-muted-foreground">
            <a href={links.source} target="_blank" rel="noreferrer">
              <GithubMark className="size-4" />
              View on GitHub
            </a>
          </Button>
        </Reveal>

        <Reveal delay={0.16} className="mt-14 w-full max-w-4xl">
          <HeroDemo />
        </Reveal>
      </div>
    </section>
  )
}
