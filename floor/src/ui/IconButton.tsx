import { LoaderCircle } from 'lucide-react';
import type { ReactNode } from 'react';

import { buttonClass, isWaiting, whileIdle } from './Button.tsx';
import type { ButtonProps } from './Button.tsx';
import { ICON } from './look.ts';

export type IconButtonProps = Omit<ButtonProps, 'children' | 'icon' | 'aria-label' | 'aria-labelledby'> & {
  /** The accessible name. An icon alone names nothing, so the type will not compile without it. */
  readonly label: string;
  readonly icon: ReactNode;
};

export function IconButton({ label, icon, variant = 'ghost', size = 'md', loading = false, className, type = 'button', onClick, ref, ...rest }: IconButtonProps) {
  const waiting = isWaiting(loading, rest['aria-disabled']);
  return (
    <button ref={ref} type={type} aria-label={label} title={label} data-ui="icon-button" data-variant={variant} data-size={size}
      data-loading={loading || undefined} {...rest} aria-busy={loading || undefined} aria-disabled={waiting || undefined}
      onClick={whileIdle(waiting, onClick)} className={buttonClass(variant, size, true, className)}>
      {loading ? <LoaderCircle {...ICON} className="animate-spin" /> : icon}
    </button>
  );
}
