import { HeadContent, Outlet, createRootRoute } from '@tanstack/react-router'

import { Footer } from '@/components/footer'
import { Nav } from '@/components/nav'
import { ThemeProvider } from '@/components/theme-provider'
import { TooltipProvider } from '@/components/ui/tooltip'

function RootLayout() {
  return (
    <ThemeProvider>
      <TooltipProvider delayDuration={200}>
        <HeadContent />
        <a
          href="#main"
          className="sr-only focus:not-sr-only focus:fixed focus:top-3 focus:left-3 focus:z-[60] focus:rounded-full focus:bg-primary focus:px-4 focus:py-2 focus:text-sm focus:font-medium focus:text-primary-foreground"
        >
          Skip to content
        </a>
        <Nav />
        <main id="main">
          <Outlet />
        </main>
        <Footer />
      </TooltipProvider>
    </ThemeProvider>
  )
}

export const Route = createRootRoute({ component: RootLayout })
