import { motion, useReducedMotion } from 'motion/react'

type RevealProps = {
  children: React.ReactNode
  className?: string
  /** Seconds of stagger; siblings pass 0, 0.06, 0.12 and so on. */
  delay?: number
  as?: 'div' | 'section' | 'li' | 'span'
}

/** Scroll reveal that collapses to its end state under prefers-reduced-motion. */
export function Reveal({ children, className, delay = 0, as = 'div' }: RevealProps) {
  const reduced = useReducedMotion()
  const Tag = motion[as]

  if (reduced) return <Tag className={className}>{children}</Tag>

  return (
    <Tag
      className={className}
      initial={{ opacity: 0, y: 12 }}
      whileInView={{ opacity: 1, y: 0 }}
      viewport={{ once: true, margin: '-64px' }}
      transition={{ duration: 0.55, delay, ease: [0.22, 1, 0.36, 1] }}
    >
      {children}
    </Tag>
  )
}
