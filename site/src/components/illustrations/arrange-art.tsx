import { useRef } from 'react'
import { motion, useInView, useReducedMotion } from 'motion/react'

const PERIOD = 5.6
const EASE: [number, number, number, number] = [0.22, 1, 0.36, 1]

/** Keyframe stops given in seconds, normalised to the loop period. */
const times = (...seconds: number[]) => seconds.map((second) => second / PERIOD)

/** Step two: the stray display glides flush against its neighbour and the layout is remembered. */
export function ArrangeArt() {
  const ref = useRef<SVGSVGElement>(null)
  const inView = useInView(ref, { amount: 0.5 })
  const reduced = useReducedMotion() ?? false
  const run = inView && !reduced
  const settle = { duration: reduced ? 0 : 0.35, ease: EASE }

  return (
    <svg ref={ref} viewBox="0 0 320 192" aria-hidden className="h-auto w-full">
      <defs>
        <radialGradient id="arrange-wash" cx="50%" cy="0%" r="88%">
          <stop offset="0%" stopColor="var(--glow)" stopOpacity="0.26" />
          <stop offset="100%" stopColor="var(--glow)" stopOpacity="0" />
        </radialGradient>
        <clipPath id="arrange-panel-1">
          <rect x="32" y="61" width="72" height="45" rx="5" />
        </clipPath>
        <clipPath id="arrange-panel-2">
          <rect x="104" y="54" width="92" height="52" rx="5" />
        </clipPath>
        <clipPath id="arrange-panel-3">
          <rect x="196" y="54" width="92" height="52" rx="5" />
        </clipPath>
      </defs>

      <path
        d="M28 106H296"
        className="stroke-foreground/12"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeDasharray="2 6"
      />
      <rect
        x="196"
        y="54"
        width="92"
        height="52"
        rx="5"
        fill="none"
        className="stroke-foreground/14"
        strokeWidth="1.5"
        strokeDasharray="4 5"
      />

      <rect
        x="32"
        y="61"
        width="72"
        height="45"
        rx="5"
        className="fill-foreground/6 stroke-border"
        strokeWidth="1.5"
      />
      <g clipPath="url(#arrange-panel-1)">
        <rect x="32" y="61" width="72" height="45" fill="url(#arrange-wash)" />
      </g>
      <text x="68" y="89" textAnchor="middle" fontSize="15" fontWeight={600} className="fill-foreground/28 font-sans">
        1
      </text>

      <rect
        x="104"
        y="54"
        width="92"
        height="52"
        rx="5"
        className="fill-foreground/6 stroke-border"
        strokeWidth="1.5"
      />
      <g clipPath="url(#arrange-panel-2)">
        <rect x="104" y="54" width="92" height="52" fill="url(#arrange-wash)" />
      </g>
      <text x="150" y="86" textAnchor="middle" fontSize="15" fontWeight={600} className="fill-foreground/28 font-sans">
        2
      </text>

      <motion.g
        animate={run ? { x: [16, 16, 0, 0, 16, 16], y: [-9, -9, 0, 0, -9, -9] } : { x: 0, y: 0 }}
        transition={
          run
            ? {
                duration: PERIOD,
                times: times(0, 0.7, 1.75, 4.6, 5.5, PERIOD),
                repeat: Infinity,
                ease: EASE,
              }
            : settle
        }
      >
        <rect
          x="196"
          y="54"
          width="92"
          height="52"
          rx="5"
          className="fill-card stroke-border"
          strokeWidth="1.5"
        />
        <g clipPath="url(#arrange-panel-3)">
          <rect x="196" y="54" width="92" height="52" fill="url(#arrange-wash)" />
        </g>
        <text x="242" y="86" textAnchor="middle" fontSize="15" fontWeight={600} className="fill-foreground/28 font-sans">
          3
        </text>
      </motion.g>

      <motion.g
        animate={run ? { opacity: [0, 0, 1, 0.5, 0.5, 0, 0] } : { opacity: 0.5 }}
        transition={
          run
            ? {
                duration: PERIOD,
                times: times(0, 1.65, 1.95, 2.4, 4.2, 4.7, PERIOD),
                repeat: Infinity,
                ease: 'easeOut',
              }
            : settle
        }
      >
        <path d="M196 54V106" className="stroke-primary/30" strokeWidth="6" strokeLinecap="round" />
        <path d="M196 54V106" className="stroke-primary" strokeWidth="2" strokeLinecap="round" />
      </motion.g>

      <motion.g
        animate={run ? { opacity: [0, 0, 1, 1, 0, 0], y: [8, 8, 0, 0, 4, 8] } : { opacity: 1, y: 0 }}
        transition={
          run
            ? {
                duration: PERIOD,
                times: times(0, 2, 2.5, 4.2, 4.7, PERIOD),
                repeat: Infinity,
                ease: EASE,
              }
            : settle
        }
      >
        <rect
          x="200"
          y="126"
          width="104"
          height="22"
          rx="11"
          className="fill-primary/10 stroke-primary/30"
          strokeWidth="1.5"
        />
        <circle cx="212" cy="137" r="3" className="fill-primary" />
        <text x="224" y="141" fontSize="9" letterSpacing="1.5" fill="var(--ink)" className="font-mono">
          REMEMBERED
        </text>
      </motion.g>
    </svg>
  )
}
