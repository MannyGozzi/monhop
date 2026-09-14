import { useRef } from 'react'
import { motion, useInView, useReducedMotion } from 'motion/react'

const PERIOD = 5.6
const EASE: [number, number, number, number] = [0.22, 1, 0.36, 1]
/** The pointer reaches the seam roughly here on the way over and on the way back. */
const CROSS_OUT = 1.45
const CROSS_BACK = 4.45

/** Keyframe stops given in seconds, normalised to the loop period. */
const times = (...seconds: number[]) => seconds.map((second) => second / PERIOD)

type PulseProps = { at: number; run: boolean; reduced: boolean }

function SeamPulse({ at, run, reduced }: PulseProps) {
  return (
    <motion.g
      style={{ transformBox: 'fill-box', transformOrigin: 'center' }}
      animate={
        run
          ? { opacity: [0, 0.8, 0.55, 0, 0], scale: [0.35, 0.5, 1.1, 2.2, 2.2] }
          : { opacity: 0, scale: 2.2 }
      }
      transition={
        run
          ? {
              duration: PERIOD,
              times: times(0, at, at + 0.12, at + 0.75, PERIOD),
              repeat: Infinity,
              ease: 'easeOut',
            }
          : { duration: reduced ? 0 : 0.35, ease: EASE }
      }
    >
      <circle cx="160" cy="70" r="11" className="fill-primary/15" />
      <circle
        cx="160"
        cy="70"
        r="11"
        fill="none"
        className="stroke-primary"
        strokeWidth="1.5"
        vectorEffect="non-scaling-stroke"
      />
    </motion.g>
  )
}

/** Step three: the pointer crosses the shared seam and the keyboard lands on the other machine. */
export function CrossArt() {
  const ref = useRef<SVGSVGElement>(null)
  const inView = useInView(ref, { amount: 0.5 })
  const reduced = useReducedMotion() ?? false
  const run = inView && !reduced
  const settle = { duration: reduced ? 0 : 0.35, ease: EASE }
  const washTimes = times(0, 1.2, 1.7, 4.2, 4.7, PERIOD)

  return (
    <svg ref={ref} viewBox="0 0 320 192" aria-hidden className="h-auto w-full">
      <defs>
        <radialGradient id="cross-wash-a" cx="30%" cy="6%" r="88%">
          <stop offset="0%" stopColor="var(--glow)" stopOpacity="0.28" />
          <stop offset="100%" stopColor="var(--glow)" stopOpacity="0" />
        </radialGradient>
        <radialGradient id="cross-wash-b" cx="70%" cy="94%" r="88%">
          <stop offset="0%" stopColor="var(--glow)" stopOpacity="0.24" />
          <stop offset="100%" stopColor="var(--glow)" stopOpacity="0" />
        </radialGradient>
        <clipPath id="cross-screen-a">
          <rect x="24" y="34" width="136" height="77" rx="7" />
        </clipPath>
        <clipPath id="cross-screen-b">
          <rect x="160" y="34" width="136" height="77" rx="7" />
        </clipPath>
      </defs>

      <rect
        x="24"
        y="34"
        width="136"
        height="77"
        rx="7"
        className="fill-foreground/6 stroke-border"
        strokeWidth="1.5"
      />
      <g clipPath="url(#cross-screen-a)">
        <rect x="24" y="34" width="136" height="77" fill="url(#cross-wash-a)" />
        <rect x="24" y="34" width="136" height="8" className="fill-foreground/10" />
        <circle cx="31" cy="38" r="1.6" className="fill-foreground/35" />
        <rect x="37" y="37" width="18" height="2" rx="1" className="fill-foreground/22" />
        <motion.rect
          x="24"
          y="34"
          width="136"
          height="77"
          className="fill-primary/10"
          animate={run ? { opacity: [1, 1, 0, 0, 1, 1] } : { opacity: 0 }}
          transition={
            run ? { duration: PERIOD, times: washTimes, repeat: Infinity, ease: EASE } : settle
          }
        />
      </g>

      <rect
        x="160"
        y="34"
        width="136"
        height="77"
        rx="7"
        className="fill-foreground/6 stroke-border"
        strokeWidth="1.5"
      />
      <g clipPath="url(#cross-screen-b)">
        <rect x="160" y="34" width="136" height="77" fill="url(#cross-wash-b)" />
        <rect x="160" y="103" width="136" height="8" className="fill-foreground/10" />
        <rect x="210" y="106" width="3" height="3" rx="0.6" className="fill-foreground/35" />
        <rect x="216" y="106" width="3" height="3" rx="0.6" className="fill-foreground/22" />
        <rect x="222" y="106" width="3" height="3" rx="0.6" className="fill-foreground/22" />
        <motion.rect
          x="160"
          y="34"
          width="136"
          height="77"
          className="fill-primary/10"
          animate={run ? { opacity: [0, 0, 1, 1, 0, 0] } : { opacity: 1 }}
          transition={
            run ? { duration: PERIOD, times: washTimes, repeat: Infinity, ease: EASE } : settle
          }
        />
      </g>

      <path d="M160 34V111" className="stroke-foreground/18" strokeWidth="1.5" />
      <motion.path
        d="M160 34V111"
        className="stroke-primary"
        strokeWidth="1.5"
        animate={run ? { opacity: [0.18, 0.18, 0.9, 0.35, 0.35, 0.9, 0.18, 0.18] } : { opacity: 0.35 }}
        transition={
          run
            ? {
                duration: PERIOD,
                times: times(0, 1.25, 1.55, 2.1, 4.2, 4.5, 4.9, PERIOD),
                repeat: Infinity,
                ease: 'easeOut',
              }
            : settle
        }
      />

      <SeamPulse at={CROSS_OUT} run={run} reduced={reduced} />
      <SeamPulse at={CROSS_BACK} run={run} reduced={reduced} />

      <motion.g
        animate={
          run ? { x: [62, 62, 244, 244, 62, 62], y: [84, 84, 58, 58, 84, 84] } : { x: 244, y: 58 }
        }
        transition={
          run
            ? {
                duration: PERIOD,
                times: times(0, 0.8, 2, 4, 5, PERIOD),
                repeat: Infinity,
                ease: EASE,
              }
            : settle
        }
      >
        <g transform="scale(0.9)">
          <path
            d="M1.2 1.1 13.4 11.3c.5.4.2 1.2-.4 1.2l-5 .2 2.5 4.9c.2.4 0 .8-.4 1l-1.2.6c-.4.2-.8 0-1-.4l-2.5-4.9-3.3 3.3c-.4.4-1.2.1-1.2-.5V1.7c0-.7.8-1 1.3-.6Z"
            className="fill-foreground"
            stroke="var(--background)"
            strokeWidth="1"
            strokeLinejoin="round"
          />
        </g>
      </motion.g>

      {[
        { x: 206, lit: times(0, 1.6, 1.85, 4.3, 4.55, PERIOD) },
        { x: 232, lit: times(0, 1.95, 2.2, 4.15, 4.4, PERIOD) },
      ].map((cap) => (
        <motion.g
          key={cap.x}
          style={{ transformBox: 'fill-box', transformOrigin: 'center' }}
          animate={run ? { scale: [1, 1, 0.94, 0.94, 1, 1] } : { scale: 0.94 }}
          transition={
            run
              ? { duration: PERIOD, times: cap.lit, repeat: Infinity, ease: EASE }
              : settle
          }
        >
          <rect
            x={cap.x}
            y="124"
            width="18"
            height="18"
            rx="5"
            className="fill-foreground/8 stroke-border"
            strokeWidth="1.5"
          />
          <motion.rect
            x={cap.x}
            y="124"
            width="18"
            height="18"
            rx="5"
            className="fill-primary"
            animate={run ? { opacity: [0, 0, 1, 1, 0, 0] } : { opacity: 1 }}
            transition={
              run
                ? { duration: PERIOD, times: cap.lit, repeat: Infinity, ease: EASE }
                : settle
            }
          />
        </motion.g>
      ))}

      <text x="228" y="158" textAnchor="middle" fontSize="10" className="fill-muted-foreground font-sans">
        Keyboard follows
      </text>
    </svg>
  )
}
