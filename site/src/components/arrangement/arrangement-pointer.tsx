/** Same arrow as the hero demo, so one cursor reads across the whole page. */
export function ArrangementPointer({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 20" aria-hidden className={className}>
      <path
        d="M1.2 1.1 13.4 11.3c.5.4.2 1.2-.4 1.2l-5 .2 2.5 4.9c.2.4 0 .8-.4 1l-1.2.6c-.4.2-.8 0-1-.4l-2.5-4.9-3.3 3.3c-.4.4-1.2.1-1.2-.5V1.7c0-.7.8-1 1.3-.6Z"
        fill="currentColor"
        stroke="var(--background)"
        strokeWidth="1"
        strokeLinejoin="round"
      />
    </svg>
  )
}
