import { LoaderCircle } from 'lucide-react';
import { useId } from 'react';
import type { ReactNode, Ref } from 'react';

import { cn } from './cn.ts';
import { ICON, RING } from './look.ts';

export interface SwitchProps {
  readonly checked: boolean;
  readonly onCheckedChange: (checked: boolean) => void;
  readonly label: ReactNode;
  readonly description?: ReactNode;
  /** The daemon is applying the change. The switch keeps showing what it was handed and ignores presses. */
  readonly busy?: boolean;
  readonly disabled?: boolean;
  readonly className?: string;
  readonly ref?: Ref<HTMLButtonElement>;
}

/**
 * A setting that is on or off, with the words that name it. It draws only what it is handed, so a setting the daemon
 * keeps never shows a value the daemon does not hold.
 */
export function Switch({ checked, onCheckedChange, label, description, busy = false, disabled = false, className, ref }: SwitchProps) {
  const made = useId();
  const labelId = `${made}-label`;
  const descriptionId = `${made}-description`;
  return (
    <div data-ui="switch-row" className={cn('flex items-start justify-between gap-4 font-sans', className)}>
      <div className="min-w-0">
        <span id={labelId} className="text-base font-medium text-ink">{label}</span>
        {description === undefined ? null : <p id={descriptionId} className="m-0 mt-0.5 text-small text-muted">{description}</p>}
      </div>
      <button ref={ref} type="button" role="switch" aria-checked={checked} aria-labelledby={labelId}
        aria-describedby={description === undefined ? undefined : descriptionId} aria-busy={busy || undefined}
        aria-disabled={busy || undefined} disabled={disabled} data-ui="switch" data-state={checked ? 'on' : 'off'}
        onClick={() => { if (!busy) onCheckedChange(!checked); }}
        className={cn(
          'group relative mt-0.5 box-border inline-flex h-5 min-h-6 w-9 max-[801px]:h-11 max-[801px]:border-y-10 max-[801px]:border-solid max-[801px]:border-transparent max-[801px]:bg-clip-padding shrink-0 cursor-pointer items-center rounded-pill border-0 p-0.5',
          'bg-line-strong transition-[color,background-color,border-color,opacity] hover:bg-muted on:bg-accent on:hover:bg-accent-hover',
          'active:opacity-90 disabled:cursor-not-allowed disabled:opacity-50 waiting:cursor-progress waiting:opacity-70', RING)}>
        <span data-part="knob" className={cn(
          'pointer-events-none flex size-4 items-center justify-center rounded-pill bg-surface text-muted shadow-card',
          'transition-transform group-on:translate-x-4')}>
          {busy ? <LoaderCircle size={12} strokeWidth={ICON.strokeWidth} className="animate-spin" /> : null}
        </span>
      </button>
    </div>
  );
}
