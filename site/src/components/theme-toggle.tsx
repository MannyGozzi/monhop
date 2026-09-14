import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { Laptop, Moon, Sun } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import { useTheme, type ThemePreference } from '@/lib/theme'

const icons: Record<ThemePreference, typeof Sun> = { system: Laptop, light: Sun, dark: Moon }
const labels: Record<ThemePreference, string> = {
  system: 'Theme: follows your system',
  light: 'Theme: light',
  dark: 'Theme: dark',
}

export function ThemeToggle({ className }: { className?: string }) {
  const { preference, cycle } = useTheme()
  const reduced = useReducedMotion()
  const Icon = icons[preference]

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          variant="ghost"
          size="icon-sm"
          className={className}
          aria-label={labels[preference]}
          onClick={cycle}
        >
          <AnimatePresence initial={false} mode="wait">
            <motion.span
              key={preference}
              initial={reduced ? false : { opacity: 0, rotate: -45, scale: 0.7 }}
              animate={{ opacity: 1, rotate: 0, scale: 1 }}
              exit={reduced ? undefined : { opacity: 0, rotate: 45, scale: 0.7 }}
              transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
              className="flex"
            >
              <Icon aria-hidden />
            </motion.span>
          </AnimatePresence>
        </Button>
      </TooltipTrigger>
      <TooltipContent>{labels[preference]}</TooltipContent>
    </Tooltip>
  )
}
