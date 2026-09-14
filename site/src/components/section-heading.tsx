import { cn } from '@/lib/utils'

type SectionHeadingProps = {
  title: React.ReactNode
  description?: React.ReactNode
  className?: string
  align?: 'center' | 'start'
}

export function SectionHeading({
  title,
  description,
  className,
  align = 'center',
}: SectionHeadingProps) {
  return (
    <div
      className={cn(
        'flex max-w-2xl flex-col gap-3',
        align === 'center' ? 'mx-auto items-center text-center' : 'items-start text-left',
        className,
      )}
    >
      <h2 className="text-balance text-3xl font-semibold tracking-tight sm:text-4xl">{title}</h2>
      {description ? (
        <p className="text-pretty text-base/7 text-muted-foreground">{description}</p>
      ) : null}
    </div>
  )
}
