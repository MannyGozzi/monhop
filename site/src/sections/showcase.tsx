import { ArrowLeftRight, Move, MonitorSmartphone } from 'lucide-react'

import { ArrangementShowcase } from '@/components/arrangement'
import { Reveal } from '@/components/reveal'
import { SectionHeading } from '@/components/section-heading'

const captions = [
  {
    icon: Move,
    title: 'Any edge',
    body: 'Left, right, top or bottom. Whichever edges you put together are the ones you cross.',
  },
  {
    icon: ArrowLeftRight,
    title: 'Snap to touch',
    body: 'The edges you want to cross only need to touch. There is nothing else to line up.',
  },
  {
    icon: MonitorSmartphone,
    title: 'One arrangement per setup, per computer',
    body: 'Laptop alone, docked at home, docked at work, for every computer you pair: each remembered.',
  },
]

export function Showcase() {
  return (
    <section id="showcase" className="relative px-5 py-20 sm:px-8 sm:py-28">
      <div className="mx-auto w-full max-w-6xl">
        <Reveal>
          <SectionHeading
            title="Arrange once. MonHop remembers."
            description="Drag your displays until the edges you cross are touching. MonHop keeps one arrangement per display setup for every computer you pair, and switches the moment you plug in or unplug."
          />
        </Reveal>

        <Reveal delay={0.08} className="mt-12 sm:mt-16">
          <ArrangementShowcase />
        </Reveal>

        <ul className="mt-8 grid gap-4 sm:grid-cols-3">
          {captions.map((caption, index) => {
            const Icon = caption.icon
            return (
              <Reveal as="li" key={caption.title} delay={index * 0.06} className="flex gap-3">
                <span className="flex size-8 shrink-0 items-center justify-center rounded-lg border border-border bg-background/70 text-[var(--ink)]">
                  <Icon aria-hidden className="size-4" />
                </span>
                <span className="flex flex-col gap-1">
                  <span className="text-sm font-semibold tracking-tight">{caption.title}</span>
                  <span className="text-sm/6 text-muted-foreground">{caption.body}</span>
                </span>
              </Reveal>
            )
          })}
        </ul>
      </div>
    </section>
  )
}
