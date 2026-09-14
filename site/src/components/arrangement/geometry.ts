export type DisplayId = 'studio' | 'macbook' | 'windows'
export type SceneId = 'loose' | 'docked' | 'alone'
export type PointerStop = 'park' | 'onLoose' | 'onDocked' | 'aim' | 'crossed'
export type VariantId = 'wide' | 'tall'
export type SavedId = 'docked' | 'alone'

export type Point = { x: number; y: number }
export type Rect = { x: number; y: number; w: number; h: number }
/** A shared edge: `axis` names the constant coordinate, `from`/`to` the span along the other one. */
export type Seam = { axis: 'x' | 'y'; at: number; from: number; to: number }

export type Scene = { pos: Record<DisplayId, Point>; seam: Seam | null }

export type Variant = {
  width: number
  height: number
  size: Record<DisplayId, { w: number; h: number }>
  scenes: Record<SceneId, Scene>
  pointer: Record<PointerStop, Point>
}

export const DISPLAY_IDS = ['studio', 'macbook', 'windows'] as const

export const displayMeta: Record<
  DisplayId,
  { label: string; caption: string; owner: 'mac' | 'windows' }
> = {
  studio: { label: 'Studio Display', caption: '27-inch · 16:9', owner: 'mac' },
  macbook: { label: 'MacBook Pro', caption: '16-inch · 16:10', owner: 'mac' },
  windows: { label: 'Windows PC', caption: '27-inch · 16:9', owner: 'windows' },
}

/** Landscape canvas: the loose monitor docks against a vertical edge. */
const wide: Variant = {
  width: 1000,
  height: 440,
  size: {
    studio: { w: 300, h: 169 },
    macbook: { w: 224, h: 140 },
    windows: { w: 284, h: 160 },
  },
  scenes: {
    loose: {
      pos: { studio: { x: 200, y: 58 }, macbook: { x: 238, y: 227 }, windows: { x: 612, y: 84 } },
      seam: null,
    },
    docked: {
      pos: { studio: { x: 200, y: 58 }, macbook: { x: 238, y: 227 }, windows: { x: 500, y: 84 } },
      seam: { axis: 'x', at: 500, from: 84, to: 227 },
    },
    alone: {
      pos: { studio: { x: 200, y: 58 }, macbook: { x: 240, y: 148 }, windows: { x: 464, y: 138 } },
      seam: { axis: 'x', at: 464, from: 148, to: 288 },
    },
  },
  pointer: {
    park: { x: 935, y: 405 },
    onLoose: { x: 735, y: 150 },
    onDocked: { x: 623, y: 150 },
    aim: { x: 430, y: 150 },
    crossed: { x: 575, y: 150 },
  },
}

/** Portrait canvas for phones: the same story on a horizontal edge, so the boxes stay readable. */
const tall: Variant = {
  width: 560,
  height: 760,
  size: {
    studio: { w: 320, h: 180 },
    macbook: { w: 264, h: 165 },
    windows: { w: 300, h: 169 },
  },
  scenes: {
    loose: {
      pos: { studio: { x: 120, y: 120 }, macbook: { x: 148, y: 300 }, windows: { x: 200, y: 560 } },
      seam: null,
    },
    docked: {
      pos: { studio: { x: 120, y: 120 }, macbook: { x: 148, y: 300 }, windows: { x: 130, y: 465 } },
      seam: { axis: 'y', at: 465, from: 148, to: 412 },
    },
    alone: {
      pos: { studio: { x: 120, y: 120 }, macbook: { x: 148, y: 215 }, windows: { x: 130, y: 380 } },
      seam: { axis: 'y', at: 380, from: 148, to: 412 },
    },
  },
  pointer: {
    park: { x: 505, y: 735 },
    onLoose: { x: 350, y: 645 },
    onDocked: { x: 280, y: 550 },
    aim: { x: 280, y: 400 },
    crossed: { x: 280, y: 525 },
  },
}

export const variants: Record<VariantId, Variant> = { wide, tall }

/** Below this canvas width the landscape arrangement would shrink the display labels past reading. */
export const TALL_BELOW = 600

export type Step = {
  id: string
  /** Seconds this step holds before the machine advances. */
  hold: number
  scene: SceneId
  move: number
  pointer: PointerStop
  pointerVisible: boolean
  pointerMove: number
  lift: boolean
  studioIn: boolean
  seam: 'idle' | 'flash' | 'pulse'
  status: string | null
  touching: boolean
  crossing: boolean
  saved: SavedId[]
  active: SavedId | null
  caption: string
}

const base = {
  move: 0.3,
  pointer: 'park' as PointerStop,
  pointerVisible: false,
  pointerMove: 0.3,
  lift: false,
  studioIn: true,
  seam: 'idle' as const,
  status: null,
  touching: false,
  crossing: false,
  saved: [] as SavedId[],
  active: null,
  caption: 'Drag a display until the edges you want to cross are touching.',
}

const both: SavedId[] = ['docked', 'alone']

export const STEPS: Step[] = [
  { ...base, id: 'idle', hold: 0.5, scene: 'loose', move: 0.55 },
  { ...base, id: 'reach', hold: 1.1, scene: 'loose', pointer: 'onLoose', pointerVisible: true, pointerMove: 1 },
  { ...base, id: 'grab', hold: 0.4, scene: 'loose', pointer: 'onLoose', pointerVisible: true, pointerMove: 0.2, lift: true },
  { ...base, id: 'drag', hold: 1.4, scene: 'docked', move: 1.3, pointer: 'onDocked', pointerVisible: true, pointerMove: 1.3, lift: true },
  { ...base, id: 'snap', hold: 0.7, scene: 'docked', pointer: 'onDocked', pointerVisible: true, pointerMove: 0.25, seam: 'flash', touching: true, caption: 'Edges touching. The pointer can cross here.' },
  { ...base, id: 'aim', hold: 0.45, scene: 'docked', pointer: 'aim', pointerVisible: true, pointerMove: 0.4, touching: true, caption: 'Edges touching. The pointer can cross here.' },
  { ...base, id: 'cross', hold: 1.05, scene: 'docked', pointer: 'crossed', pointerVisible: true, pointerMove: 1, seam: 'pulse', touching: true, crossing: true, caption: 'The pointer crosses into the Windows PC and the keyboard follows.' },
  { ...base, id: 'remember', hold: 0.9, scene: 'docked', pointer: 'crossed', touching: true, status: 'Remembered', saved: ['docked'], active: 'docked', caption: 'Saved for this display setup. No need to arrange it again.' },
  { ...base, id: 'unplug', hold: 0.8, scene: 'docked', studioIn: false, status: 'Studio Display unplugged', saved: ['docked'], active: 'docked', caption: 'A display goes away.' },
  { ...base, id: 'reflow', hold: 1.3, scene: 'alone', move: 1.2, studioIn: false, seam: 'flash', status: 'Switched automatically', saved: both, active: 'alone', caption: 'MonHop switches to the laptop-alone arrangement by itself.' },
  { ...base, id: 'aloneRest', hold: 1.8, scene: 'alone', studioIn: false, saved: both, active: 'alone', caption: 'MonHop switches to the laptop-alone arrangement by itself.' },
  { ...base, id: 'replug', hold: 1, scene: 'docked', move: 0.95, status: 'Studio Display plugged in', saved: both, active: 'docked', caption: 'The display comes back.' },
  { ...base, id: 'restore', hold: 1.1, scene: 'docked', seam: 'flash', status: 'Switched automatically', saved: both, active: 'docked', caption: 'The docked arrangement is back, without touching anything.' },
  { ...base, id: 'rest', hold: 0.7, scene: 'docked', saved: both, active: 'docked', caption: 'The docked arrangement is back, without touching anything.' },
  { ...base, id: 'reset', hold: 0.8, scene: 'loose', move: 0.7 },
]

/** Still frame for reduced motion: docked, touching, both arrangements already saved. */
export const REST_INDEX = STEPS.findIndex((step) => step.id === 'rest')

export const EASE = [0.22, 1, 0.36, 1] as const

/** Distance, in rendered pixels, within which a released display snaps flush. */
export const SNAP_PX = 24

export function sharedEdge(a: Rect, b: Rect): Seam | null {
  const eps = 1.5
  const overlapY = Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y)
  const overlapX = Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x)
  if (overlapY > 1) {
    if (Math.abs(a.x + a.w - b.x) < eps)
      return { axis: 'x', at: b.x, from: Math.max(a.y, b.y), to: Math.min(a.y + a.h, b.y + b.h) }
    if (Math.abs(b.x + b.w - a.x) < eps)
      return { axis: 'x', at: a.x, from: Math.max(a.y, b.y), to: Math.min(a.y + a.h, b.y + b.h) }
  }
  if (overlapX > 1) {
    if (Math.abs(a.y + a.h - b.y) < eps)
      return { axis: 'y', at: b.y, from: Math.max(a.x, b.x), to: Math.min(a.x + a.w, b.x + b.w) }
    if (Math.abs(b.y + b.h - a.y) < eps)
      return { axis: 'y', at: a.y, from: Math.max(a.x, b.x), to: Math.min(a.x + a.w, b.x + b.w) }
  }
  return null
}

/** Nearest flush position for `moving` against `others`, or null when nothing is within SNAP_PX. */
export function nearestSnap(moving: Rect, others: Rect[]): Point | null {
  const candidates: { point: Point; distance: number }[] = []
  for (const other of others) {
    const overlapY = Math.min(moving.y + moving.h, other.y + other.h) - Math.max(moving.y, other.y)
    const overlapX = Math.min(moving.x + moving.w, other.x + other.w) - Math.max(moving.x, other.x)
    if (overlapY > 8) {
      candidates.push(
        { point: { x: other.x - moving.w, y: moving.y }, distance: Math.abs(moving.x + moving.w - other.x) },
        { point: { x: other.x + other.w, y: moving.y }, distance: Math.abs(moving.x - other.x - other.w) },
      )
    }
    if (overlapX > 8) {
      candidates.push(
        { point: { x: moving.x, y: other.y - moving.h }, distance: Math.abs(moving.y + moving.h - other.y) },
        { point: { x: moving.x, y: other.y + other.h }, distance: Math.abs(moving.y - other.y - other.h) },
      )
    }
  }
  const near = candidates.filter((candidate) => candidate.distance <= SNAP_PX)
  if (near.length === 0) return null
  return near.reduce((a, b) => (b.distance < a.distance ? b : a)).point
}

export type ThumbRect = { x: number; y: number; w: number; h: number; accent: boolean }

export const savedArrangements: Record<SavedId, { label: string; rects: ThumbRect[] }> = {
  docked: {
    label: 'Docked · 3 displays',
    rects: [
      { x: 6, y: 6, w: 45, h: 46, accent: true },
      { x: 12, y: 54, w: 34, h: 38, accent: true },
      { x: 51, y: 13, w: 43, h: 43, accent: false },
    ],
  },
  alone: {
    label: 'Laptop alone · 2 displays',
    rects: [
      { x: 6, y: 11, w: 39, h: 77, accent: true },
      { x: 45, y: 6, w: 49, h: 88, accent: false },
    ],
  },
}
