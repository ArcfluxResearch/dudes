import { cn } from './cn.ts';

/** Lucide's size and stroke for every icon the components draw. */
export const ICON = { size: 16, strokeWidth: 1.75 } as const;

const RING_BASE = 'outline-0 focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-focus';

/** The one focus ring, shown for keyboard focus only. */
export const RING = cn(RING_BASE, 'focus-visible:outline-offset-2');

/** For a control inside a box that clips: the ring is drawn inside the control instead of outside it. */
export const RING_INSET = cn(RING_BASE, 'focus-visible:-outline-offset-2');
