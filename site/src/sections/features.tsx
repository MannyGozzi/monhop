import {
  ArrowLeftRight,
  ChevronsLeftRight,
  ClipboardCopy,
  Lock,
  Monitor,
  Mouse,
  ShieldCheck,
  SunDim,
  WifiOff,
} from 'lucide-react'
import { motion, useReducedMotion } from 'motion/react'

import { Reveal } from '@/components/reveal'
import { SectionHeading } from '@/components/section-heading'

const features = [
  {
    icon: WifiOff,
    title: 'Local only',
    body: 'Keystrokes, pointer moves and clipboard contents never leave your network. No cloud, no account, no telemetry.',
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
    body: 'Mac to Windows, Mac to Mac, Windows to Windows. Pair up to 16 computers and share with up to eight at once, all in one arrangement.',
  },
  {
    icon: ClipboardCopy,
    title: 'Shared clipboard',
    body: 'Copy text or an image on one computer and paste it on another. Off until you turn it on, and its contents are never logged.',
  },
  {
    icon: ChevronsLeftRight,
    title: 'Swipe between pages',
    body: 'A quick two-finger swipe on a Mac trackpad goes back or forward on the computer it controls, with a chevron at the pointer to show it landed.',
  },
  {
    icon: Mouse,
    title: 'Middle-click autoscroll',
    body: 'A Windows mouse autoscrolls a Mac with its middle button, the way it does on Windows: hold and drag, or click once and move.',
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
