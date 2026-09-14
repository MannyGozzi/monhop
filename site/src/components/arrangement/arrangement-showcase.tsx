import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import {
  AnimatePresence,
  animate,
  motion,
  useInView,
  useMotionValue,
  useReducedMotion,
} from 'motion/react'

import { cn } from '@/lib/utils'
import { ArrangementDisplay } from './arrangement-display'
import { ArrangementPointer } from './arrangement-pointer'
import {
  DISPLAY_IDS,
  EASE,
  REST_INDEX,
  STEPS,
  TALL_BELOW,
  nearestSnap,
  savedArrangements,
  sharedEdge,
  variants,
  type DisplayId,
  type Rect,
  type SavedId,
  type Seam,
  type ThumbRect,
  type VariantId,
} from './geometry'

/** Seconds of quiet after a drag before the loop restarts from the top. */
const RESUME_AFTER = 6

function clamp(value: number, min: number, max: number) {
  return Math.min(Math.max(value, min), max)
}

function SeamLine({ seam, mode }: { seam: Seam; mode: 'idle' | 'flash' | 'pulse' }) {
  const vertical = seam.axis === 'x'
  const thickness = 3
  const box = vertical
    ? {
        left: seam.at - thickness / 2,
        top: seam.from,
        width: thickness,
        height: Math.max(seam.to - seam.from, 1),
      }
    : {
        left: seam.from,
        top: seam.at - thickness / 2,
        width: Math.max(seam.to - seam.from, 1),
        height: thickness,
      }
  const opacity = mode === 'flash' ? [0, 1, 0.45] : mode === 'pulse' ? [0.4, 1, 0.4] : 0.38
  const along = mode === 'flash' ? [0.2, 1, 1] : 1
  const initial = vertical ? { opacity: 0, scaleY: 0.2 } : { opacity: 0, scaleX: 0.2 }
  const target = vertical ? { opacity, scaleY: along } : { opacity, scaleX: along }

  return (
    <motion.span
      className="pointer-events-none absolute z-20 rounded-full bg-primary"
      style={{ ...box, boxShadow: '0 0 12px 1px var(--primary)' }}
      initial={initial}
      animate={target}
      exit={{ opacity: 0 }}
      transition={
        mode === 'pulse'
          ? { duration: 1.1, repeat: Infinity, ease: 'easeInOut' }
          : { duration: mode === 'flash' ? 0.6 : 0.3, ease: EASE }
      }
    />
  )
}

function Chip({
  children,
  lit,
  className,
}: {
  children: React.ReactNode
  lit: boolean
  className?: string
}) {
  return (
    <span
      className={cn(
        'relative inline-flex items-center gap-1.5 rounded-full border border-border bg-card/70 px-2.5 py-1 text-[11px] font-medium whitespace-nowrap sm:text-xs',
        className,
      )}
    >
      <motion.span
        aria-hidden
        className="absolute inset-0 rounded-full bg-primary/12 ring-1 ring-primary/55"
        animate={{ opacity: lit ? 1 : 0 }}
        transition={{ duration: 0.3, ease: EASE }}
      />
      <span className="relative flex items-center gap-1.5">{children}</span>
    </span>
  )
}

function Thumb({ rects }: { rects: ThumbRect[] }) {
  return (
    <span className="relative block h-5 w-8 shrink-0 overflow-hidden rounded-[4px] border border-border bg-background/70">
      {rects.map((rect) => (
        <span
          key={`${rect.x}-${rect.y}`}
          className={cn('absolute rounded-[1px]', rect.accent ? 'bg-primary/75' : 'bg-foreground/35')}
          style={{ left: `${rect.x}%`, top: `${rect.y}%`, width: `${rect.w}%`, height: `${rect.h}%` }}
        />
      ))}
    </span>
  )
}

export function ArrangementShowcase({ className }: { className?: string }) {
  const reduced = useReducedMotion() ?? false
  const tileRef = useRef<HTMLDivElement>(null)
  const canvasRef = useRef<HTMLDivElement>(null)
  const inView = useInView(tileRef, { amount: 0.3 })

  // A fresh object every time, so resuming at step 0 still restarts the loop and the moves.
  // `saved` accumulates across loops: an arrangement stays in the strip once it was remembered.
  const [cursor, setCursor] = useState<{ index: number; saved: SavedId[] }>({ index: 0, saved: [] })
  const [interacting, setInteracting] = useState(false)
  const [userSeam, setUserSeam] = useState<{ id: number; seam: Seam } | null>(null)
  const [box, setBox] = useState<{ width: number; variant: VariantId }>({ width: 0, variant: 'wide' })
  // Dragging owns the touch gesture, so coarse pointers keep it and scroll the page instead.
  const [draggable, setDraggable] = useState(false)

  const resumeTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const seamCount = useRef(0)
  const lastLayout = useRef('')

  const studioX = useMotionValue(0)
  const studioY = useMotionValue(0)
  const macbookX = useMotionValue(0)
  const macbookY = useMotionValue(0)
  const windowsX = useMotionValue(0)
  const windowsY = useMotionValue(0)
  const values = useMemo(
    () => ({
      studio: { x: studioX, y: studioY },
      macbook: { x: macbookX, y: macbookY },
      windows: { x: windowsX, y: windowsY },
    }),
    [studioX, studioY, macbookX, macbookY, windowsX, windowsY],
  )

  const variant = variants[box.variant]
  const scale = box.width ? box.width / variant.width : 0
  const step = STEPS[reduced ? REST_INDEX : cursor.index]
  const scene = variant.scenes[step.scene]
  const running = inView && !reduced && !interacting

  useLayoutEffect(() => {
    const node = canvasRef.current
    if (!node) return
    const measure = (width: number) => {
      const next: VariantId = width < TALL_BELOW ? 'tall' : 'wide'
      setBox((prev) => (prev.width === width && prev.variant === next ? prev : { width, variant: next }))
    }
    measure(node.getBoundingClientRect().width)
    const observer = new ResizeObserver(([entry]) => measure(entry.contentRect.width))
    observer.observe(node)
    return () => observer.disconnect()
  }, [])

  // One timer at a time: each step schedules only its own successor.
  useEffect(() => {
    if (!running) return
    const timer = setTimeout(
      () =>
        setCursor((current) => {
          const index = (current.index + 1) % STEPS.length
          const fresh = STEPS[index].saved.filter((id) => !current.saved.includes(id))
          return { index, saved: fresh.length ? [...current.saved, ...fresh] : current.saved }
        }),
      STEPS[cursor.index].hold * 1000,
    )
    return () => clearTimeout(timer)
  }, [running, cursor])

  // Positions are motion values so dragging and the loop write to the same place.
  useEffect(() => {
    if (!scale) return
    const layout = `${box.variant}:${scale}`
    const jump = lastLayout.current !== layout
    lastLayout.current = layout
    const current = STEPS[reduced ? REST_INDEX : cursor.index]
    const target = variants[box.variant].scenes[current.scene]
    const options = jump ? { duration: 0 } : { duration: current.move, ease: EASE }
    const controls = DISPLAY_IDS.flatMap((id) => [
      animate(values[id].x, target.pos[id].x * scale, options),
      animate(values[id].y, target.pos[id].y * scale, options),
    ])
    return () => {
      for (const control of controls) control.stop()
    }
  }, [scale, box.variant, cursor, reduced, values])

  useEffect(() => {
    const query = window.matchMedia('(hover: hover) and (pointer: fine)')
    const sync = () => setDraggable(query.matches)
    sync()
    query.addEventListener('change', sync)
    return () => query.removeEventListener('change', sync)
  }, [])

  useEffect(() => () => {
    if (resumeTimer.current) clearTimeout(resumeTimer.current)
  }, [])

  const rectOf = useCallback(
    (id: DisplayId): Rect => ({
      x: values[id].x.get(),
      y: values[id].y.get(),
      w: variant.size[id].w * scale,
      h: variant.size[id].h * scale,
    }),
    [values, variant, scale],
  )

  const handleGrab = useCallback(() => {
    if (resumeTimer.current) clearTimeout(resumeTimer.current)
    setInteracting(true)
  }, [])

  const handleRelease = useCallback(
    (id: DisplayId) => {
      if (!scale) return
      const moving = rectOf(id)
      const others = DISPLAY_IDS.filter(
        (other) => other !== id && (other !== 'studio' || step.studioIn),
      ).map(rectOf)
      const snap = nearestSnap(moving, others)
      if (snap) {
        const placed = {
          ...moving,
          x: clamp(snap.x, 0, variant.width * scale - moving.w),
          y: clamp(snap.y, 0, variant.height * scale - moving.h),
        }
        const options = { duration: 0.26, ease: EASE }
        animate(values[id].x, placed.x, options)
        animate(values[id].y, placed.y, options)
        if (!reduced) {
          const edge = others.map((other) => sharedEdge(placed, other)).find(Boolean)
          if (edge) setUserSeam({ id: ++seamCount.current, seam: edge })
        }
      }
      if (reduced) {
        setInteracting(false)
        return
      }
      resumeTimer.current = setTimeout(() => {
        setInteracting(false)
        setCursor((current) => ({ index: 0, saved: current.saved }))
      }, RESUME_AFTER * 1000)
    },
    [rectOf, scale, step.studioIn, values, variant, reduced],
  )

  useEffect(() => {
    if (!userSeam) return
    const timer = setTimeout(() => setUserSeam(null), 800)
    return () => clearTimeout(timer)
  }, [userSeam])

  const saved: SavedId[] = reduced ? ['docked', 'alone'] : cursor.saved
  const caption = interacting
    ? 'Drop it next to another display and it snaps into place.'
    : reduced
      ? 'Every arrangement is saved for its display setup and restored by itself.'
      : step.caption
  const active = reduced ? 'docked' : step.active
  const ghost = step.pointerVisible && !interacting && !reduced
  const pointerStop = variant.pointer[step.pointer]
  const sceneSeam: Seam | null =
    scale && scene.seam
      ? {
          axis: scene.seam.axis,
          at: scene.seam.at * scale,
          from: scene.seam.from * scale,
          to: scene.seam.to * scale,
        }
      : null
  const touching = reduced || step.touching
  const radius = Math.max(6, 16 * scale)

  return (
    <div
      ref={tileRef}
      className={cn('glass relative overflow-hidden rounded-3xl p-3 sm:p-5', className)}
    >
      <span
        aria-hidden
        className="pointer-events-none absolute inset-0 opacity-70"
        style={{
          background:
            'radial-gradient(70% 55% at 50% 0%, color-mix(in oklab, var(--glow) 16%, transparent), transparent 70%)',
        }}
      />

      <div
        ref={canvasRef}
        role="img"
        aria-label="A display arrangement editor: a Windows monitor is dragged until its edge touches a Mac display, the pointer crosses the shared edge, and each arrangement is saved and restored when a display is plugged in or unplugged."
        className="dot-grid relative w-full overflow-hidden rounded-2xl border border-border bg-background/45"
        style={{ aspectRatio: `${variant.width} / ${variant.height}` }}
      >
        {scale > 0 ? (
          <>
            {DISPLAY_IDS.map((id) => (
              <ArrangementDisplay
                key={id}
                id={id}
                x={values[id].x}
                y={values[id].y}
                width={variant.size[id].w * scale}
                height={variant.size[id].h * scale}
                radius={radius}
                draggable={draggable}
                present={id !== 'studio' || step.studioIn}
                lifted={id === 'windows' && step.lift}
                primary={id === (step.studioIn ? 'studio' : 'macbook')}
                constraints={{
                  left: 0,
                  top: 0,
                  right: (variant.width - variant.size[id].w) * scale,
                  bottom: (variant.height - variant.size[id].h) * scale,
                }}
                onGrab={handleGrab}
                onRelease={handleRelease}
              />
            ))}

            {sceneSeam ? <SeamLine seam={sceneSeam} mode={step.seam} /> : null}

            <AnimatePresence>
              {userSeam ? <SeamLine key={userSeam.id} seam={userSeam.seam} mode="flash" /> : null}
            </AnimatePresence>

            {sceneSeam ? (
              <div
                className="pointer-events-none absolute z-30"
                style={{
                  left: sceneSeam.axis === 'x' ? sceneSeam.at : sceneSeam.to,
                  top: sceneSeam.axis === 'x' ? sceneSeam.from : sceneSeam.at,
                  transform: 'translate(-50%, -130%)',
                }}
              >
                <motion.span
                  className="inline-flex"
                  initial={false}
                  animate={{ opacity: touching ? 1 : 0, scale: touching ? 1 : 0.85 }}
                  transition={{ duration: 0.3, ease: EASE }}
                >
                  <Chip lit>
                    <span className="size-1.5 rounded-full bg-primary" />
                    Touching
                  </Chip>
                </motion.span>
              </div>
            ) : null}

            <motion.span
              aria-hidden
              className="pointer-events-none absolute top-0 left-0 z-40 text-foreground drop-shadow-[0_2px_6px_oklch(0_0_0/35%)]"
              initial={false}
              animate={{ x: pointerStop.x * scale, y: pointerStop.y * scale, opacity: ghost ? 1 : 0 }}
              transition={{
                duration: step.pointerMove,
                ease: EASE,
                opacity: { duration: 0.3, ease: EASE },
              }}
            >
              <ArrangementPointer className="size-4 sm:size-5" />
            </motion.span>

            <div className="pointer-events-none absolute top-2.5 right-2.5 z-30 flex justify-end sm:top-3 sm:right-3">
              <AnimatePresence mode="wait">
                {step.status && !interacting ? (
                  <motion.span
                    key={step.status}
                    className="inline-flex"
                    initial={{ opacity: 0, y: -6 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={{ opacity: 0, y: -6 }}
                    transition={{ duration: 0.28, ease: EASE }}
                  >
                    <Chip lit>{step.status}</Chip>
                  </motion.span>
                ) : null}
              </AnimatePresence>
            </div>
          </>
        ) : null}
      </div>

      <div className="relative mt-3 flex flex-wrap items-center gap-2">
        <Chip lit>
          <span className="size-1.5 rounded-full bg-primary" />
          This Mac · Input
        </Chip>
        <Chip lit={false}>
          <span className="size-1.5 rounded-full bg-foreground/40" />
          Windows PC
        </Chip>
        <Chip lit={step.crossing}>
          <span className="size-1.5 rounded-full bg-primary/70" />
          Pointer crossing
        </Chip>
      </div>

      <div className="relative mt-3 flex flex-col gap-2 rounded-2xl border border-border bg-card/55 px-3 py-2.5 sm:flex-row sm:items-center sm:gap-4">
        <div className="flex min-h-9 min-w-0 flex-1 items-center gap-2">
          <span className="size-1.5 shrink-0 rounded-full bg-primary" />
          <AnimatePresence initial={false} mode="wait">
            <motion.p
              key={caption}
              className="text-[13px]/5 text-muted-foreground sm:text-sm/5"
              initial={{ opacity: 0, y: 6 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, y: -6 }}
              transition={{ duration: 0.22, ease: EASE }}
            >
              {caption}
            </motion.p>
          </AnimatePresence>
        </div>
        <div className="flex min-w-0 flex-nowrap items-center gap-2 overflow-x-auto py-0.5 sm:ml-auto">
          <AnimatePresence initial={false}>
            {saved.map((id) => (
              <motion.span
                key={id}
                className="relative inline-flex shrink-0 items-center gap-2 rounded-xl border border-border bg-background/70 px-2 py-1.5"
                initial={{ opacity: 0, x: 12 }}
                animate={{ opacity: 1, x: 0 }}
                exit={{ opacity: 0, x: 12 }}
                transition={{ duration: 0.4, ease: EASE }}
              >
                <motion.span
                  aria-hidden
                  className="absolute inset-0 rounded-xl bg-primary/8 ring-1 ring-primary/55"
                  animate={{ opacity: active === id ? 1 : 0 }}
                  transition={{ duration: 0.3, ease: EASE }}
                />
                <span className="relative flex items-center gap-2">
                  <Thumb rects={savedArrangements[id].rects} />
                  <span className="text-[11px] font-medium whitespace-nowrap sm:text-xs">
                    {savedArrangements[id].label}
                  </span>
                </span>
              </motion.span>
            ))}
          </AnimatePresence>
        </div>
      </div>
    </div>
  )
}
