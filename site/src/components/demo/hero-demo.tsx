import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import {
  AnimatePresence,
  animate,
  motion,
  useMotionValue,
  useMotionValueEvent,
  useReducedMotion,
  useSpring,
  useTransform,
} from 'motion/react'

import { cn } from '@/lib/utils'
import { DemoDisplay } from './demo-display'
import { DemoPointer } from './demo-pointer'
import type { DemoCaret, DemoPeer, DemoSide, HeroDemoProps } from './types'

/** Pointer rest position on each display, as a fraction of the track box. */
const restPoint: Record<DemoSide, { x: number; y: number }> = {
  mac: { x: 0.27, y: 0.58 },
  windows: { x: 0.73, y: 0.4 },
}

/** The line each screen writes once the pointer lands on it. */
const sceneLine: Record<DemoSide, string> = {
  mac: 'Back on the MacBook. Same keys.',
  windows: 'Typed from the MacBook keyboard.',
}

const defaultLabels = { mac: 'MacBook Pro', windows: 'Windows PC' }
const otherPeers: DemoPeer[] = [
  { name: 'Mac mini', os: 'mac' },
  { name: 'Work laptop', os: 'windows' },
]

const msPerChar = 62
const msBeforeTyping = 180
/** How long the pointer is home before the roster hands the far screen to the next computer. */
const msBeforeSwitch = 560
const landingEase: [number, number, number, number] = [0.55, 0, 0.12, 1]
const settleEase: [number, number, number, number] = [0.22, 1, 0.36, 1]
const rippleEase: [number, number, number, number] = [0.16, 1, 0.3, 1]
const trailNear = { stiffness: 320, damping: 30, mass: 0.6 }
const trailFar = { stiffness: 190, damping: 26, mass: 0.75 }
const tiltSpring = { stiffness: 140, damping: 18, mass: 0.5 }
const maxTilt = 4

function useTrackSize(ref: React.RefObject<HTMLDivElement | null>) {
  const [size, setSize] = useState({ width: 0, height: 0 })
  useLayoutEffect(() => {
    const node = ref.current
    if (!node) return
    const observer = new ResizeObserver(([entry]) => {
      const box = entry.contentRect
      setSize({ width: box.width, height: box.height })
    })
    observer.observe(node)
    return () => observer.disconnect()
  }, [ref])
  return size
}

export function HeroDemo({
  className,
  dwellSeconds = 3.6,
  paused = false,
  initialSide = 'mac',
  restSide = 'windows',
  labels = defaultLabels,
  peers,
  onSideChange,
}: HeroDemoProps) {
  const reduced = useReducedMotion() ?? false
  const frozen = reduced || paused
  const crossSeconds = Math.min(0.9, dwellSeconds * 0.45)
  const [loopTarget, setLoopTarget] = useState<DemoSide>(initialSide)
  const [loopLanded, setLoopLanded] = useState<DemoSide>(initialSide)
  const [typedCount, setTypedCount] = useState(0)
  const [peerIndex, setPeerIndex] = useState(0)
  const [seamHit, setSeamHit] = useState(0)
  const [tiltable, setTiltable] = useState(false)
  const trackRef = useRef<HTMLDivElement>(null)
  const track = useTrackSize(trackRef)

  const roster = peers?.length ? peers : [{ name: labels.windows, os: 'windows' as const }, ...otherPeers]
  // The pointer leads; the keyboard, the glow, the roster and the pill follow once it has landed.
  const target = frozen ? restSide : loopTarget
  const side = frozen ? restSide : loopLanded
  const peerCursor = frozen ? 0 : peerIndex % roster.length
  const peer = roster[peerCursor]
  const line = sceneLine[side]
  const count = frozen ? line.length : typedCount
  const typed = line.slice(0, count)
  const typing = count > 0 && count < line.length
  const here = side === 'mac' ? labels.mac : peer.name

  const caretFor = (screen: DemoSide): DemoCaret =>
    screen !== side ? 'idle' : frozen || typing ? 'solid' : 'blink'

  useEffect(() => {
    if (frozen) return
    const timer = setInterval(
      () => setLoopTarget((current) => (current === 'mac' ? 'windows' : 'mac')),
      dwellSeconds * 1000,
    )
    return () => clearInterval(timer)
  }, [frozen, dwellSeconds])

  // Landing hands the line to the new screen, so the count resets with the same beat.
  useEffect(() => {
    if (frozen) return
    const timer = setTimeout(() => {
      setLoopLanded(loopTarget)
      setTypedCount(0)
    }, crossSeconds * 1000)
    return () => clearTimeout(timer)
  }, [frozen, loopTarget, crossSeconds])

  // One self-terminating timeout per character, so the loop holds no interval once the line is written.
  useEffect(() => {
    if (frozen || typedCount >= line.length) return
    const timer = setTimeout(
      () => setTypedCount((written) => written + 1),
      typedCount === 0 ? msBeforeTyping : msPerChar,
    )
    return () => clearTimeout(timer)
  }, [frozen, typedCount, line.length])

  // Arriving home ends a round, and the roster hands the far screen to the next computer.
  const cameFrom = useRef<DemoSide>(initialSide)
  useEffect(() => {
    if (frozen) return
    const previous = cameFrom.current
    cameFrom.current = side
    if (side !== 'mac' || previous === 'mac') return
    const timer = setTimeout(
      () => setPeerIndex((current) => (current + 1) % roster.length),
      msBeforeSwitch,
    )
    return () => clearTimeout(timer)
  }, [frozen, side, roster.length])

  useEffect(() => onSideChange?.(side), [side, onSideChange])

  const pointerX = useMotionValue(0)
  const pointerY = useMotionValue(0)
  const nearX = useSpring(pointerX, trailNear)
  const nearY = useSpring(pointerY, trailNear)
  const farX = useSpring(pointerX, trailFar)
  const farY = useSpring(pointerY, trailFar)
  const tiltX = useSpring(0, tiltSpring)
  const tiltY = useSpring(0, tiltSpring)
  const glowX = useTransform(tiltY, [-maxTilt, maxTilt], [-16, 16])
  const glowY = useTransform(tiltX, [-maxTilt, maxTilt], [12, -12])

  const point = restPoint[target]
  const lastTarget = useRef<DemoSide | null>(null)
  const pastSeam = useRef(false)

  useEffect(() => {
    if (!track.width || !track.height) return
    const x = track.width * point.x
    const y = track.height * point.y
    const crossing = !frozen && lastTarget.current !== null && lastTarget.current !== target
    lastTarget.current = target
    if (!crossing) {
      pastSeam.current = x > track.width / 2
      pointerX.set(x)
      pointerY.set(y)
      return
    }
    const runX = animate(pointerX, x, { duration: crossSeconds, ease: landingEase })
    const runY = animate(pointerY, y, { duration: crossSeconds, ease: landingEase })
    return () => {
      runX.stop()
      runY.stop()
    }
  }, [target, point.x, point.y, track.width, track.height, frozen, crossSeconds, pointerX, pointerY])

  useMotionValueEvent(pointerX, 'change', (value) => {
    if (frozen || !track.width) return
    const past = value > track.width / 2
    if (past === pastSeam.current) return
    pastSeam.current = past
    setSeamHit((hit) => hit + 1)
  })

  useEffect(() => {
    const query = window.matchMedia('(hover: hover) and (pointer: fine)')
    const sync = () => setTiltable(query.matches)
    sync()
    query.addEventListener('change', sync)
    return () => query.removeEventListener('change', sync)
  }, [])

  const tilting = tiltable && !frozen
  useEffect(() => {
    if (tilting) return
    tiltX.set(0)
    tiltY.set(0)
  }, [tilting, tiltX, tiltY])

  function leanTowards(event: React.PointerEvent<HTMLDivElement>) {
    if (!tilting) return
    const box = event.currentTarget.getBoundingClientRect()
    tiltY.set(((event.clientX - box.left) / box.width - 0.5) * maxTilt * 2)
    tiltX.set((0.5 - (event.clientY - box.top) / box.height) * maxTilt * 2)
  }

  function lieFlat() {
    tiltX.set(0)
    tiltY.set(0)
  }

  const ripple = !frozen && seamHit > 0

  return (
    <div
      className={cn('relative', className)}
      style={{ perspective: '1400px' }}
      onPointerMove={leanTowards}
      onPointerLeave={lieFlat}
    >
      <motion.div
        className="glass relative overflow-hidden rounded-3xl p-4 sm:p-6"
        style={{ rotateX: tiltX, rotateY: tiltY }}
        role="img"
        aria-label={`One keyboard reaching several computers: a pointer crosses from the ${labels.mac} onto ${peer.name}, the next line types itself there, and the paired computers below show which one is live.`}
      >
        <motion.div
          aria-hidden
          className="pointer-events-none absolute inset-0"
          style={{ x: glowX, y: glowY }}
        >
          <motion.div
            className="absolute -inset-[14%]"
            style={{
              background:
                'radial-gradient(52% 46% at 50% 14%, color-mix(in oklab, var(--glow) 24%, transparent), transparent 70%)',
            }}
            initial={{ opacity: 0.58, x: '-2%', y: '0%' }}
            animate={
              frozen
                ? undefined
                : { opacity: [0.5, 0.78, 0.5], x: ['-2%', '2%', '-2%'], y: ['0%', '3%', '0%'] }
            }
            transition={frozen ? undefined : { duration: 18, repeat: Infinity, ease: 'easeInOut' }}
          />
        </motion.div>

        <div ref={trackRef} className="relative flex gap-3 sm:gap-5">
          <DemoDisplay
            os="mac"
            app="editor"
            sceneKey="home"
            label={labels.mac}
            active={side === 'mac'}
            focused={target === 'mac'}
            reduced={frozen}
            typed={side === 'mac' ? typed : ''}
            caret={caretFor('mac')}
          />

          <div className="flex shrink-0 flex-col self-stretch pb-7">
            <div className="relative w-0.5 flex-1">
              <span
                aria-hidden
                className="absolute inset-0 rounded-full bg-primary opacity-35"
                style={{ boxShadow: '0 0 12px color-mix(in oklab, var(--primary) 55%, transparent)' }}
              />
              {ripple ? (
                <motion.span
                  key={seamHit}
                  aria-hidden
                  className="absolute inset-0 rounded-full bg-primary"
                  style={{
                    boxShadow: '0 0 20px color-mix(in oklab, var(--primary) 80%, transparent)',
                  }}
                  initial={{ opacity: 0.9, scaleX: 2.6 }}
                  animate={{ opacity: 0, scaleX: 1 }}
                  transition={{ duration: 0.62, ease: rippleEase }}
                />
              ) : null}
            </div>
          </div>

          <DemoDisplay
            os={peer.os}
            app="chat"
            sceneKey={`${peer.os}-${peer.name}`}
            label={peer.name}
            active={side === 'windows'}
            focused={target === 'windows'}
            reduced={frozen}
            typed={side === 'windows' ? typed : ''}
            caret={caretFor('windows')}
          />

          {ripple ? (
            <motion.div
              aria-hidden
              className="pointer-events-none absolute top-0 left-0 z-10"
              style={{ x: track.width / 2, y: pointerY }}
            >
              <div className="relative size-16 -translate-x-1/2 -translate-y-1/2 sm:size-24">
                <motion.span
                  key={`ring-${seamHit}`}
                  className="absolute inset-0 rounded-full border border-primary/55"
                  initial={{ scale: 0.18, opacity: 0.85 }}
                  animate={{ scale: 1, opacity: 0 }}
                  transition={{ duration: 0.8, ease: rippleEase }}
                />
                <motion.span
                  key={`wash-${seamHit}`}
                  className="absolute inset-0 rounded-full"
                  style={{
                    background:
                      'radial-gradient(closest-side, color-mix(in oklab, var(--primary) 38%, transparent), transparent 76%)',
                  }}
                  initial={{ scale: 0.3, opacity: 0.6 }}
                  animate={{ scale: 1.25, opacity: 0 }}
                  transition={{ duration: 0.7, ease: 'easeOut' }}
                />
              </div>
            </motion.div>
          ) : null}

          {frozen ? null : (
            <>
              <motion.div
                aria-hidden
                className="pointer-events-none absolute top-0 left-0 z-10 text-foreground/20"
                style={{ x: farX, y: farY }}
              >
                <DemoPointer ghost className="size-4" />
              </motion.div>
              <motion.div
                aria-hidden
                className="pointer-events-none absolute top-0 left-0 z-10 text-foreground/40"
                style={{ x: nearX, y: nearY }}
              >
                <DemoPointer ghost className="size-[1.125rem]" />
              </motion.div>
            </>
          )}

          <motion.div
            aria-hidden
            className="pointer-events-none absolute top-0 left-0 z-20 text-foreground drop-shadow-[0_2px_6px_oklch(0_0_0/35%)]"
            style={{ x: pointerX, y: pointerY }}
          >
            <DemoPointer className="size-5" />
          </motion.div>
        </div>

        <div className="relative mt-4 flex flex-wrap items-center gap-x-3 gap-y-2 border-t border-border pt-4">
          <div className="flex flex-wrap items-center gap-1.5">
            {roster.map((entry, index) => {
              const live = index === peerCursor
              return (
                <motion.span
                  key={entry.name}
                  aria-hidden
                  initial={false}
                  animate={{ opacity: live ? 1 : 0.6 }}
                  transition={{ duration: frozen ? 0 : 0.3, ease: settleEase }}
                  className={cn(
                    'relative flex items-center gap-1.5 overflow-hidden rounded-full border px-2.5 py-1 text-[11px] font-medium whitespace-nowrap',
                    live ? 'border-primary/45 text-foreground' : 'border-border text-muted-foreground',
                  )}
                >
                  <motion.span
                    aria-hidden
                    className="absolute inset-0 bg-primary/12"
                    initial={false}
                    animate={{ opacity: live ? 1 : 0 }}
                    transition={{ duration: frozen ? 0 : 0.3, ease: settleEase }}
                  />
                  <span
                    className={cn(
                      'relative size-1.5 rounded-full',
                      live ? 'bg-primary' : 'bg-foreground/25',
                    )}
                    style={
                      live
                        ? { boxShadow: '0 0 8px color-mix(in oklab, var(--primary) 70%, transparent)' }
                        : undefined
                    }
                  />
                  <span className="relative">{entry.name}</span>
                </motion.span>
              )
            })}
          </div>

          <div className="ml-auto flex h-7 items-center overflow-hidden rounded-full border border-border bg-card px-3">
            <AnimatePresence initial={false} mode="wait">
              <motion.span
                key={here}
                initial={frozen ? false : { y: 10, opacity: 0 }}
                animate={{ y: 0, opacity: 1 }}
                exit={frozen ? undefined : { y: -10, opacity: 0 }}
                transition={{ duration: 0.22, ease: settleEase }}
                className="flex items-center gap-1.5 text-xs font-medium whitespace-nowrap"
              >
                <span className="size-1.5 rounded-full bg-primary" />
                Now typing on {here}
              </motion.span>
            </AnimatePresence>
          </div>
        </div>
      </motion.div>
    </div>
  )
}
