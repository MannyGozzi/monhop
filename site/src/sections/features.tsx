import { ArrowLeftRight, Lock, Monitor, ShieldCheck, SunDim, WifiOff } from 'lucide-react'
import { motion, useReducedMotion } from 'motion/react'

import { Reveal } from '@/components/reveal'
import { SectionHeading } from '@/components/section-heading'

const features = [
  {
    icon: WifiOff,
    title: 'Local only',
    body: 'Keystrokes and pointer moves never leave your network. No cloud, no account, no telemetry.',
  },
  {
    icon: Lock,
    title: 'Encrypted and pinned',
    body: 'An encrypted QUIC link bound to one network interface and one computer you verified yourself.',
  },
  {
    icon: Monitor,
    title: 'Remembers every display setup',
    body: 'Laptop alone, laptop plus monitors: each arrangement is saved and restored the moment you plug in.',
  },
  {
    icon: ArrowLeftRight,
    title: 'Any pairing, any number',
    body: 'Mac to Windows, Mac to Mac, Windows to Windows. Pair up to 16 computers and choose which one your keyboard reaches.',
  },
  {
    icon: SunDim,
    title: 'Dim every screen',
    body: 'One chord dims every display on the computer you are looking at. Control+Option+0, or Ctrl+Alt+0.',
  },
  {
    icon: ShieldCheck,
    title: 'Signed updates',
    body: 'Every update is verified against a key built into the app, and never installs while you are sharing.',
  },
]

export function Features() {
  const reduced = useReducedMotion()

  return (
    <section id="features" className="relative px-5 py-20 sm:px-8 sm:py-28">
      <div className="mx-auto w-full max-w-6xl">
        <Reveal>
          <SectionHeading
            title="Built to stay on your desk, not on a server"
            description="Everything MonHop does happens between computers you own, on a network you control."
          />
        </Reveal>

        <ul className="mt-12 grid gap-4 sm:mt-16 sm:grid-cols-2 lg:grid-cols-3">
          {features.map((feature, index) => {
            const Icon = feature.icon
            return (
              <Reveal as="li" key={feature.title} delay={(index % 3) * 0.06}>
                <motion.div
                  whileHover={reduced ? undefined : { y: -4 }}
                  transition={{ duration: 0.25, ease: [0.22, 1, 0.36, 1] }}
                  className="glass flex h-full flex-col gap-3 rounded-2xl p-5"
                >
                  <span className="flex size-9 items-center justify-center rounded-xl border border-border bg-background/70 text-[var(--ink)]">
                    <Icon aria-hidden className="size-4.5" />
                  </span>
                  <h3 className="text-base font-semibold tracking-tight">{feature.title}</h3>
                  <p className="text-sm/6 text-muted-foreground">{feature.body}</p>
                </motion.div>
              </Reveal>
            )
          })}
        </ul>
      </div>
    </section>
  )
}
