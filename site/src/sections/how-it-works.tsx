import { ArrangeArt, CrossArt, PairArt } from '@/components/illustrations'
import { Reveal } from '@/components/reveal'
import { SectionHeading } from '@/components/section-heading'
import { cn } from '@/lib/utils'

const steps = [
  {
    art: PairArt,
    title: 'Pair once',
    body: 'Two computers on the same network show the same short code. Confirm it once and they stay paired, up to 16 per computer.',
  },
  {
    art: ArrangeArt,
    title: 'Arrange your displays',
    body: 'Drag the displays until the edges you want to cross are touching, the way you would in system settings.',
  },
  {
    art: CrossArt,
    title: 'Cross the edge',
    body: 'Push the pointer past that edge and it appears on the other computer. The keyboard goes with it.',
  },
]

export function HowItWorks() {
  return (
    <section id="how-it-works" className="dot-grid relative px-5 py-20 sm:px-8 sm:py-28">
      <div className="mx-auto w-full max-w-6xl">
        <Reveal>
          <SectionHeading
            title="Three steps, then you forget it is there"
            description="Setup takes about a minute and only happens once. After that MonHop starts with your computers and stays out of the way."
          />
        </Reveal>

        <ol className="mt-12 grid gap-4 sm:mt-16 sm:grid-cols-3">
          {steps.map((step, index) => {
            const Art = step.art
            return (
              <Reveal as="li" key={step.title} delay={index * 0.07}>
                <div className={cn('glass flex h-full flex-col gap-4 rounded-2xl p-5')}>
                  <div className="rounded-xl border border-border bg-background/60 p-2">
                    <Art />
                  </div>
                  <div className="flex flex-col gap-1.5">
                    <h3 className="text-lg font-semibold tracking-tight">{step.title}</h3>
                    <p className="text-sm/6 text-muted-foreground">{step.body}</p>
                  </div>
                </div>
              </Reveal>
            )
          })}
        </ol>
      </div>
    </section>
  )
}
