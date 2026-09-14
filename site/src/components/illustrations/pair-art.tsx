import { useRef } from 'react'
import { motion, useInView, useReducedMotion } from 'motion/react'

const PERIOD = 5.4
const EASE: [number, number, number, number] = [0.22, 1, 0.36, 1]
/** The pairing code and each digit's centre inside a chip: three, a wider gap, then three. */
const CODE = [
  { digit: '4', dx: -30 },
  { digit: '8', dx: -19 },
  { digit: '2', dx: -8 },
  { digit: '9', dx: 8 },
  { digit: '1', dx: 19 },
  { digit: '3', dx: 30 },
]

/** Keyframe stops given in seconds, normalised to the loop period. */
const times = (...seconds: number[]) => seconds.map((second) => second / PERIOD)

type ChipProps = { cx: number; cy: number; run: boolean; reduced: boolean }

function CodeChip({ cx, cy, run, reduced }: ChipProps) {
  return (
    <>
      <rect
        x={cx - 44}
        y={cy - 14}
        width="88"
        height="28"
        rx="9"
        className="fill-primary/8 stroke-primary/35"
        strokeWidth="1.5"
      />
      {CODE.map((slot, index) => {
        const appear = 0.5 + index * 0.17
        return (
          <motion.g
            key={slot.dx}
            style={{ transformBox: 'fill-box', transformOrigin: 'center' }}
            animate={
              run ? { opacity: [0, 0, 1, 1, 0, 0], y: [3, 3, 0, 0, 0, 0] } : { opacity: 1, y: 0 }
            }
            transition={
              run
                ? {
                    duration: PERIOD,
                    times: times(0, appear, appear + 0.26, 4.1, 4.7, PERIOD),
                    repeat: Infinity,
                    ease: EASE,
                  }
                : { duration: reduced ? 0 : 0.35, ease: EASE }
            }
          >
            <text
              x={cx + slot.dx}
              y={cy + 5}
              textAnchor="middle"
              fontSize="13"
              fontWeight={600}
              className="fill-primary font-mono"
            >
              {slot.digit}
            </text>
          </motion.g>
        )
      })}
    </>
  )
}

/** Step one: the same pairing code types itself onto both machines, then the link is confirmed. */
export function PairArt() {
  const ref = useRef<SVGSVGElement>(null)
  const inView = useInView(ref, { amount: 0.5 })
  const reduced = useReducedMotion() ?? false
  const run = inView && !reduced
  const settle = { duration: reduced ? 0 : 0.35, ease: EASE }

  return (
    <svg ref={ref} viewBox="0 0 320 192" aria-hidden className="h-auto w-full">
      <defs>
        <radialGradient id="pair-wash-a" cx="28%" cy="4%" r="86%">
          <stop offset="0%" stopColor="var(--glow)" stopOpacity="0.3" />
          <stop offset="100%" stopColor="var(--glow)" stopOpacity="0" />
        </radialGradient>
        <radialGradient id="pair-wash-b" cx="72%" cy="96%" r="86%">
          <stop offset="0%" stopColor="var(--glow)" stopOpacity="0.24" />
          <stop offset="100%" stopColor="var(--glow)" stopOpacity="0" />
        </radialGradient>
        <clipPath id="pair-screen-a">
          <rect x="26" y="38" width="110" height="74" rx="7" />
        </clipPath>
        <clipPath id="pair-screen-b">
          <rect x="190" y="36" width="114" height="78" rx="7" />
        </clipPath>
      </defs>

      <rect
        x="26"
        y="38"
        width="110"
        height="74"
        rx="7"
        className="fill-foreground/6 stroke-border"
        strokeWidth="1.5"
      />
      <g clipPath="url(#pair-screen-a)">
        <rect x="26" y="38" width="110" height="74" fill="url(#pair-wash-a)" />
        <rect x="26" y="38" width="110" height="8" className="fill-foreground/10" />
        <circle cx="33" cy="42" r="1.6" className="fill-foreground/35" />
        <rect x="39" y="41" width="18" height="2" rx="1" className="fill-foreground/22" />
      </g>
      <rect
        x="16"
        y="114"
        width="130"
        height="9"
        rx="4.5"
        className="fill-foreground/10 stroke-border"
        strokeWidth="1.5"
      />
      <path d="M66 118.5h30" className="stroke-foreground/22" strokeWidth="1.5" strokeLinecap="round" />

      <rect
        x="190"
        y="36"
        width="114"
        height="78"
        rx="7"
        className="fill-foreground/6 stroke-border"
        strokeWidth="1.5"
      />
      <g clipPath="url(#pair-screen-b)">
        <rect x="190" y="36" width="114" height="78" fill="url(#pair-wash-b)" />
        <rect x="190" y="106" width="114" height="8" className="fill-foreground/10" />
        <rect x="240" y="109" width="3" height="3" rx="0.6" className="fill-foreground/35" />
        <rect x="246" y="109" width="3" height="3" rx="0.6" className="fill-foreground/22" />
        <rect x="252" y="109" width="3" height="3" rx="0.6" className="fill-foreground/22" />
      </g>
      <rect x="240" y="114" width="14" height="12" className="fill-foreground/10" />
      <rect
        x="222"
        y="126"
        width="50"
        height="6"
        rx="3"
        className="fill-foreground/10 stroke-border"
        strokeWidth="1.5"
      />

      <CodeChip cx={81} cy={75} run={run} reduced={reduced} />
      <CodeChip cx={247} cy={75} run={run} reduced={reduced} />

      <path
        d="M151 75H136"
        className="stroke-foreground/15"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeDasharray="3 4"
      />
      <path
        d="M175 75H190"
        className="stroke-foreground/15"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeDasharray="3 4"
      />

      <motion.circle
        cx="163"
        cy="75"
        r="14"
        className="fill-primary/12"
        style={{ transformBox: 'fill-box', transformOrigin: 'center' }}
        animate={
          run
            ? { opacity: [0, 0, 1, 1, 0, 0], scale: [0.6, 0.6, 1, 1, 0.9, 0.9] }
            : { opacity: 1, scale: 1 }
        }
        transition={
          run
            ? {
                duration: PERIOD,
                times: times(0, 1.7, 2.05, 4.1, 4.7, PERIOD),
                repeat: Infinity,
                ease: EASE,
              }
            : settle
        }
      />
      <motion.path
        d="M156.5 75.5l4.5 4.5 9-10"
        fill="none"
        className="stroke-primary"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
        animate={
          run ? { pathLength: [0, 0, 1, 1, 0, 0], opacity: [0, 0, 1, 1, 0, 0] } : { pathLength: 1, opacity: 1 }
        }
        transition={
          run
            ? {
                duration: PERIOD,
                times: times(0, 1.75, 2.35, 4.1, 4.7, PERIOD),
                repeat: Infinity,
                ease: EASE,
              }
            : settle
        }
      />
      {['M151 75H136', 'M175 75H190'].map((d) => (
        <motion.path
          key={d}
          d={d}
          fill="none"
          className="stroke-primary"
          strokeWidth="1.5"
          strokeLinecap="round"
          animate={
            run ? { pathLength: [0, 0, 1, 1, 0, 0], opacity: [0, 0, 1, 1, 0, 0] } : { pathLength: 1, opacity: 1 }
          }
          transition={
            run
              ? {
                  duration: PERIOD,
                  times: times(0, 2.25, 2.7, 4.1, 4.7, PERIOD),
                  repeat: Infinity,
                  ease: EASE,
                }
              : settle
          }
        />
      ))}

      <text x="81" y="152" textAnchor="middle" fontSize="11" className="fill-muted-foreground font-sans">
        MacBook
      </text>
      <text x="247" y="152" textAnchor="middle" fontSize="11" className="fill-muted-foreground font-sans">
        Windows PC
      </text>
    </svg>
  )
}
