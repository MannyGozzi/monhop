import { createFileRoute } from '@tanstack/react-router'

import { Download } from '@/sections/download'
import { Faq } from '@/sections/faq'
import { Features } from '@/sections/features'
import { Hero } from '@/sections/hero'
import { HowItWorks } from '@/sections/how-it-works'
import { Showcase } from '@/sections/showcase'

function LandingPage() {
  return (
    <>
      <Hero />
      <HowItWorks />
      <Features />
      <Showcase />
      <Download />
      <Faq />
    </>
  )
}

export const Route = createFileRoute('/')({ component: LandingPage })
