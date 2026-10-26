export function cx(...parts: readonly (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(' ');
}

/** The one focus ring, shown for keyboard focus only. */
const RING_BASE = 'outline-0 focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-focus';
export const RING = `${RING_BASE} focus-visible:outline-offset-2`;
/** For a control inside a box that clips: the ring is drawn inside the control instead of outside it. */
export const RING_INSET = `${RING_BASE} focus-visible:-outline-offset-2`;

export const ICON = { size: 16, strokeWidth: 1.75 } as const;
