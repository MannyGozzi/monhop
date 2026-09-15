import { useEffect, useState } from 'react'
import { Menu } from 'lucide-react'

import { GithubMark } from '@/components/brand-marks'
import { ThemeToggle } from '@/components/theme-toggle'
import { Button } from '@/components/ui/button'
import {
  Sheet,
  SheetClose,
  SheetContent,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from '@/components/ui/sheet'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import { cn } from '@/lib/utils'
import { links, product } from '@/lib/links'

const sections = [
  { href: '/#features', label: 'Features' },
  { href: '/#how-it-works', label: 'How it works' },
  { href: '/#download', label: 'Download' },
  { href: '/changelog', label: 'Release notes' },
]

export function Nav() {
  const [lifted, setLifted] = useState(false)

  useEffect(() => {
    const onScroll = () => setLifted(globalThis.scrollY > 12)
    onScroll()
    globalThis.addEventListener('scroll', onScroll, { passive: true })
    return () => globalThis.removeEventListener('scroll', onScroll)
  }, [])

  return (
    <header className="fixed inset-x-0 top-0 z-50 flex justify-center px-4 pt-3 sm:pt-4">
      <nav
        aria-label="Main"
        className={cn(
          'glass flex w-full max-w-3xl items-center gap-1 rounded-full py-1.5 pr-1.5 pl-3 transition-shadow duration-300',
          lifted && 'shadow-[0_18px_50px_-24px_oklch(0_0_0/45%)]',
        )}
      >
        <a
          href="/"
          className="mr-auto flex items-center gap-2 rounded-full px-1 py-1 font-semibold tracking-tight focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none"
        >
          <img src="/monhop.svg" alt="" width={26} height={26} className="size-[26px] rounded-[7px]" />
          <span>{product.name}</span>
        </a>

        <ul className="hidden items-center gap-0.5 sm:flex">
          {sections.map((section) => (
            <li key={section.href}>
              <Button asChild variant="ghost" size="sm" className="rounded-full">
                <a href={section.href}>{section.label}</a>
              </Button>
            </li>
          ))}
        </ul>

        <Tooltip>
          <TooltipTrigger asChild>
            <Button asChild variant="ghost" size="icon-sm" className="rounded-full">
              <a href={links.source} target="_blank" rel="noreferrer" aria-label="MonHop on GitHub">
                <GithubMark className="size-4" />
              </a>
            </Button>
          </TooltipTrigger>
          <TooltipContent>Source on GitHub</TooltipContent>
        </Tooltip>

        <ThemeToggle className="rounded-full" />

        <Sheet>
          <SheetTrigger asChild>
            <Button variant="ghost" size="icon-sm" className="rounded-full sm:hidden" aria-label="Open menu">
              <Menu aria-hidden />
            </Button>
          </SheetTrigger>
          <SheetContent side="right" className="w-64">
            <SheetHeader>
              <SheetTitle>{product.name}</SheetTitle>
            </SheetHeader>
            <ul className="flex flex-col gap-1 px-2">
              {sections.map((section) => (
                <li key={section.href}>
                  <SheetClose asChild>
                    <a
                      href={section.href}
                      className="block rounded-lg px-3 py-2 text-sm font-medium hover:bg-muted focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none"
                    >
                      {section.label}
                    </a>
                  </SheetClose>
                </li>
              ))}
              <li>
                <a
                  href={links.source}
                  target="_blank"
                  rel="noreferrer"
                  className="flex items-center gap-2 rounded-lg px-3 py-2 text-sm font-medium hover:bg-muted focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none"
                >
                  <GithubMark className="size-4" />
                  GitHub
                </a>
              </li>
            </ul>
          </SheetContent>
        </Sheet>
      </nav>
    </header>
  )
}
