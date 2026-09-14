import { AnimatePresence, motion } from 'motion/react'

import { cn } from '@/lib/utils'
import type { DemoCaret, DemoOs } from './types'

type DemoApp = 'editor' | 'chat'

type DemoDisplayProps = {
  os: DemoOs
  app: DemoApp
  label: string
  /** Changes when this screen becomes a different computer, which cross-fades the whole scene. */
  sceneKey: string
  /** The pointer has landed here: the screen wakes and this is the live caret. */
  active: boolean
  /** The pointer is heading here, so the screen it left clears while the arrow flies. */
  focused: boolean
  reduced: boolean
  typed: string
  caret: DemoCaret
  className?: string
}

const wallpaper: Record<DemoOs, string> = {
  mac: 'radial-gradient(120% 90% at 30% 0%, color-mix(in oklab, var(--glow) 34%, transparent), transparent 70%), linear-gradient(160deg, color-mix(in oklab, var(--foreground) 6%, transparent), transparent)',
  windows:
    'radial-gradient(120% 90% at 70% 100%, color-mix(in oklab, var(--glow) 26%, transparent), transparent 70%), linear-gradient(200deg, color-mix(in oklab, var(--foreground) 6%, transparent), transparent)',
}

/** Placeholder body copy; the tone comes from the parent's text colour so scenes never hard-code a grey. */
function ScreenLine({ className }: { className?: string }) {
  return <span className={cn('block h-[3px] rounded-full bg-current md:h-1 lg:h-1.5', className)} />
}

function Caret({ mode }: { mode: DemoCaret }) {
  return (
    <motion.span
      aria-hidden
      className={cn(
        'ml-[0.06em] inline-block h-[1.02em] w-[0.1em] min-w-px rounded-[1px] align-[-0.14em]',
        mode === 'idle' ? 'bg-foreground/30' : 'bg-primary',
      )}
      initial={false}
      animate={mode === 'blink' ? { opacity: [1, 1, 0, 0] } : { opacity: 1 }}
      transition={
        mode === 'blink'
          ? { duration: 1.06, times: [0, 0.48, 0.5, 1], repeat: Infinity, ease: 'linear' }
          : { duration: 0.18 }
      }
    />
  )
}

function MacMenuBar() {
  return (
    <div className="absolute inset-x-0 top-0 flex h-[11%] items-center gap-1 bg-foreground/8 px-[3%] md:gap-2">
      <span className="size-[3px] rounded-full bg-primary/70 md:size-1.5" />
      <span className="h-[3px] w-[11%] rounded-full bg-foreground/35 md:h-1" />
      <span className="hidden h-1 w-[8%] rounded-full bg-foreground/22 md:block" />
      <span className="hidden h-1 w-[7%] rounded-full bg-foreground/22 md:block" />
      <span className="ml-auto h-[3px] w-[13%] rounded-full bg-foreground/25 md:h-1" />
    </div>
  )
}

function WindowsTaskbar() {
  return (
    <div className="absolute inset-x-0 bottom-0 flex h-[13%] items-center justify-center gap-1.5 bg-foreground/8 md:gap-2">
      <span className="size-[4px] rounded-[1px] bg-primary/70 md:size-2" />
      <span className="size-[4px] rounded-[1px] bg-foreground/35 md:size-2" />
      <span className="size-[4px] rounded-[1px] bg-foreground/22 md:size-2" />
      <span className="size-[4px] rounded-[1px] bg-foreground/22 md:size-2" />
      <span className="absolute right-[3%] h-[3px] w-[11%] rounded-full bg-foreground/25 md:h-1" />
    </div>
  )
}

function WindowButtons({ os }: { os: DemoOs }) {
  if (os === 'mac') {
    return (
      <span className="flex items-center gap-[3px] md:gap-1.5">
        <span className="size-[3px] rounded-full bg-primary/65 md:size-1.5" />
        <span className="size-[3px] rounded-full bg-foreground/25 md:size-1.5" />
        <span className="size-[3px] rounded-full bg-foreground/18 md:size-1.5" />
      </span>
    )
  }
  return (
    <span className="flex items-center gap-[5px] md:gap-2">
      <span className="h-px w-[5px] bg-foreground/35 md:w-2" />
      <span className="size-[4px] border border-foreground/35 md:size-2" />
      <span className="relative size-[4px] md:size-2">
        <span className="absolute top-1/2 left-0 h-px w-full rotate-45 bg-foreground/35" />
        <span className="absolute top-1/2 left-0 h-px w-full -rotate-45 bg-foreground/35" />
      </span>
    </span>
  )
}

/** The editor leads with the document, so its caret sits at the end of what is written. */
function EditorBody({ line }: { line: React.ReactNode }) {
  return (
    <div className="flex min-h-0 flex-1 flex-col gap-[3px] pt-[4%] text-foreground/16 md:gap-1.5 lg:gap-2">
      <ScreenLine className="w-[88%]" />
      <ScreenLine className="w-[62%] opacity-80" />
      <ScreenLine className="w-[76%] opacity-65" />
      <ScreenLine className="hidden w-[54%] opacity-50 md:block" />
      <ScreenLine className="hidden w-[82%] opacity-40 md:block" />
      {line}
    </div>
  )
}

/** The chat leads with the composer, so its caret sits at the bottom of the window. */
function ChatBody({ line }: { line: React.ReactNode }) {
  return (
    <div className="flex min-h-0 flex-1 flex-col justify-end gap-[3px] pb-[4%] text-foreground/24 md:gap-2">
      <span className="flex w-[74%] flex-col gap-[3px] self-start rounded-md rounded-bl-sm bg-foreground/8 px-1.5 py-1 md:gap-1.5 md:px-2.5 md:py-2">
        <ScreenLine className="w-full" />
        <ScreenLine className="w-[64%] opacity-70" />
      </span>
      <span className="hidden w-[58%] flex-col self-end rounded-md rounded-br-sm bg-primary/15 px-2.5 py-2 text-primary md:flex">
        <ScreenLine className="w-[84%] opacity-80" />
      </span>
      <span className="rounded-md border border-border bg-foreground/5 px-1.5 py-1 md:px-2.5 md:py-1.5">
        {line}
      </span>
    </div>
  )
}

function AppWindow({ os, app, line }: { os: DemoOs; app: DemoApp; line: React.ReactNode }) {
  return (
    <div
      className={cn(
        'absolute inset-x-[5%] flex flex-col overflow-hidden rounded-md border border-border bg-card/85',
        os === 'mac' ? 'top-[17%] bottom-[7%]' : 'top-[7%] bottom-[19%]',
      )}
      style={{ boxShadow: '0 10px 30px -22px oklch(0 0 0 / 55%)' }}
    >
      <div className="flex h-[15%] shrink-0 items-center gap-1.5 border-b border-border px-[3%]">
        {os === 'mac' ? <WindowButtons os={os} /> : null}
        <span className="hidden truncate font-mono text-[9px] text-muted-foreground md:inline">
          {app === 'editor' ? 'notes.md' : 'Messages'}
        </span>
        {os === 'windows' ? (
          <span className="ml-auto">
            <WindowButtons os={os} />
          </span>
        ) : null}
      </div>
      <div className="flex min-h-0 flex-1 flex-col overflow-hidden px-[4%]">
        {app === 'editor' ? <EditorBody line={line} /> : <ChatBody line={line} />}
      </div>
    </div>
  )
}

/** One display in the pair: a bezel, the computer's own desktop chrome and the window taking the keystrokes. */
export function DemoDisplay({
  os,
  app,
  label,
  sceneKey,
  active,
  focused,
  reduced,
  typed,
  caret,
  className,
}: DemoDisplayProps) {
  const line = (
    <motion.p
      aria-hidden
      className="min-h-[2.5em] text-[9px] leading-[1.25] font-medium tracking-[-0.01em] break-words whitespace-pre-wrap text-foreground sm:text-[10px] md:text-[11.5px] lg:text-[13px]"
      initial={false}
      animate={{ opacity: focused ? 1 : 0 }}
      transition={{ duration: reduced ? 0 : 0.34, ease: [0.22, 1, 0.36, 1] }}
    >
      {typed}
      <Caret mode={caret} />
    </motion.p>
  )

  return (
    <div className={cn('flex min-w-0 flex-1 flex-col items-center gap-2', className)}>
      <motion.div
        aria-hidden
        initial={false}
        animate={{ opacity: active ? 1 : 0.72 }}
        transition={{ duration: reduced ? 0 : 0.45, ease: [0.22, 1, 0.36, 1] }}
        className={cn(
          'relative aspect-[16/10] w-full overflow-hidden rounded-xl border bg-card',
          active ? 'border-primary/45' : 'border-border',
        )}
        style={{
          boxShadow: active
            ? '0 0 0 1px color-mix(in oklab, var(--primary) 22%, transparent), 0 20px 50px -34px var(--primary)'
            : undefined,
        }}
      >
        <AnimatePresence initial={false}>
          <motion.div
            key={sceneKey}
            className="absolute inset-0"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: reduced ? 0 : 0.42, ease: [0.22, 1, 0.36, 1] }}
          >
            <div className="absolute inset-0" style={{ background: wallpaper[os] }} />
            {os === 'mac' ? <MacMenuBar /> : <WindowsTaskbar />}
            <AppWindow os={os} app={app} line={line} />
          </motion.div>
        </AnimatePresence>
      </motion.div>

      <div className="flex h-5 items-center">
        <AnimatePresence initial={false} mode="wait">
          <motion.span
            key={label}
            initial={reduced ? false : { opacity: 0, y: 5 }}
            animate={{ opacity: 1, y: 0 }}
            exit={reduced ? undefined : { opacity: 0, y: -5 }}
            transition={{ duration: 0.22, ease: [0.22, 1, 0.36, 1] }}
            className={cn(
              'text-xs font-medium',
              active ? 'text-foreground' : 'text-muted-foreground',
            )}
          >
            {label}
          </motion.span>
        </AnimatePresence>
      </div>
    </div>
  )
}
