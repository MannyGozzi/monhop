import { motion, type MotionValue } from 'motion/react'

import { cn } from '@/lib/utils'
import { EASE, displayMeta, type DisplayId } from './geometry'

type ArrangementDisplayProps = {
  id: DisplayId
  x: MotionValue<number>
  y: MotionValue<number>
  width: number
  height: number
  radius: number
  present: boolean
  draggable: boolean
  lifted: boolean
  primary: boolean
  constraints: { left: number; top: number; right: number; bottom: number }
  onGrab: () => void
  onRelease: (id: DisplayId) => void
}

/** One panel in the arrangement: a coral outline marks the computer the keyboard is on. */
export function ArrangementDisplay({
  id,
  x,
  y,
  width,
  height,
  radius,
  present,
  draggable,
  lifted,
  primary,
  constraints,
  onGrab,
  onRelease,
}: ArrangementDisplayProps) {
  const meta = displayMeta[id]
  const isInput = meta.owner === 'mac'

  return (
    <motion.div
      drag={draggable}
      dragMomentum={false}
      dragElastic={0.04}
      dragConstraints={constraints}
      onDragStart={onGrab}
      onDragEnd={() => onRelease(id)}
      style={{ x, y, width, height, borderRadius: radius }}
      animate={{ opacity: present ? 1 : 0, scale: present ? (lifted ? 1.04 : 1) : 0.88 }}
      transition={{ duration: 0.45, ease: EASE }}
      className={cn(
        'absolute top-0 left-0 overflow-hidden border bg-card select-none',
        isInput ? 'border-primary/55' : 'border-border',
        present && draggable ? 'cursor-grab touch-none active:cursor-grabbing' : 'pointer-events-none',
      )}
    >
      <span
        className="absolute inset-0"
        style={{
          background: isInput
            ? 'radial-gradient(130% 100% at 24% 0%, color-mix(in oklab, var(--primary) 12%, transparent), transparent 68%), linear-gradient(160deg, color-mix(in oklab, var(--foreground) 5%, transparent), transparent)'
            : 'radial-gradient(130% 100% at 76% 100%, color-mix(in oklab, var(--glow) 22%, transparent), transparent 68%), linear-gradient(200deg, color-mix(in oklab, var(--foreground) 5%, transparent), transparent)',
        }}
      />

      <motion.span
        className="absolute inset-0"
        style={{
          borderRadius: radius,
          boxShadow:
            '0 0 0 1px color-mix(in oklab, var(--primary) 60%, transparent), 0 20px 46px -22px var(--primary)',
        }}
        animate={{ opacity: lifted ? 1 : 0 }}
        transition={{ duration: 0.28, ease: EASE }}
      />

      {primary ? (
        <span
          className="absolute top-[7%] left-[5%] size-1.5 rounded-full bg-primary md:size-2"
          title="Primary display"
        />
      ) : null}

      <span className="absolute inset-0 flex flex-col items-center justify-center gap-0.5 px-1 text-center">
        <span className="max-w-full truncate text-[10px] font-medium tracking-tight sm:text-xs md:text-sm">
          {meta.label}
        </span>
        <span className="max-w-full truncate text-[8px] text-muted-foreground sm:text-[10px] md:text-xs">
          {meta.caption}
        </span>
      </span>
    </motion.div>
  )
}
