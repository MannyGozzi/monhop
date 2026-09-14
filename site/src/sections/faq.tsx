import { Reveal } from '@/components/reveal'
import { SectionHeading } from '@/components/section-heading'
import {
  Accordion,
  AccordionContent,
  AccordionItem,
  AccordionTrigger,
} from '@/components/ui/accordion'

const questions = [
  {
    q: 'Does anything leave my network?',
    a: 'No. Keystrokes, pointer moves and the pairing all stay on your Wi-Fi or Ethernet, over an encrypted link pinned to one interface and one computer you verified. The only thing MonHop ever fetches from the internet is a signed update, and only when you ask for it.',
  },
  {
    q: 'Which pairings work?',
    a: 'All four: Mac to Windows, Windows to Mac, Mac to Mac and Windows to Windows. Either computer can be the one with the keyboard.',
  },
  {
    q: 'How many computers can I pair?',
    a: 'Up to 16 from each computer. Your keyboard and mouse reach one of them at a time, and you pick which one from Home. Every pair keeps its own display arrangements.',
  },
  {
    q: 'What does it cost?',
    a: 'Nothing. MonHop is free software under GPL-3.0-or-later and the source is on GitHub. Donations are welcome and never unlock anything.',
  },
  {
    q: 'Which versions do I need?',
    a: 'macOS 14 or later, on Apple silicon or Intel, and Windows 10 or 11 on 64-bit hardware.',
  },
  {
    q: 'Does it work over Wi-Fi?',
    a: 'Yes, as long as the computers are on the same network. Ethernet works too, and you pick which interface MonHop uses.',
  },
  {
    q: 'Can I use my own display layouts?',
    a: 'Yes. Arrange the displays however you like and MonHop remembers that arrangement for that display configuration. Plug in a monitor and it switches by itself.',
  },
]

export function Faq() {
  return (
    <section id="faq" className="relative px-5 py-20 sm:px-8 sm:py-28">
      <div className="mx-auto w-full max-w-3xl">
        <Reveal>
          <SectionHeading title="Before you install" />
        </Reveal>

        <Reveal delay={0.06} className="mt-10">
          <Accordion type="single" collapsible className="glass rounded-2xl px-5">
            {questions.map((entry) => (
              <AccordionItem key={entry.q} value={entry.q}>
                <AccordionTrigger className="text-base">{entry.q}</AccordionTrigger>
                <AccordionContent className="text-sm/6 text-muted-foreground">
                  {entry.a}
                </AccordionContent>
              </AccordionItem>
            ))}
          </Accordion>
        </Reveal>
      </div>
    </section>
  )
}
