/** Which screen the keyboard is on: 'mac' is the computer holding it, 'windows' is the paired computer in view. */
export type DemoSide = 'mac' | 'windows'

/** Chrome a screen wears: a Mac menu bar and traffic lights, or a Windows taskbar and window marks. */
export type DemoOs = 'mac' | 'windows'

/** Caret state: solid while keys land, blinking when idle, dim on the screen the pointer left. */
export type DemoCaret = 'solid' | 'blink' | 'idle'

/** One paired computer in the roster the keyboard can reach. */
export type DemoPeer = { name: string; os: DemoOs }

export type HeroDemoProps = {
  className?: string
  /** Seconds the pointer rests on a display before crossing the edge. */
  dwellSeconds?: number
  /** Freeze the loop; prefers-reduced-motion freezes it regardless of this value. */
  paused?: boolean
  /** Display the loop starts on. */
  initialSide?: DemoSide
  /** Display shown when the loop is frozen, so the still frame tells the whole story. */
  restSide?: DemoSide
  /** Computer names in the frames and in the status pill. */
  labels?: { mac: string; windows: string }
  /** Paired computers the loop cycles through; the first one seeds the frozen still frame. */
  peers?: DemoPeer[]
  onSideChange?: (side: DemoSide) => void
}
