import { LoaderCircle } from 'lucide-react';
import type { MouseEvent, ReactNode } from 'react';

import { buttonClass } from './Button.tsx';
import type { ButtonProps } from './Button.tsx';
import { ICON } from './look.ts';

export type IconButtonProps = Omit<ButtonProps, 'children' | 'icon' | 'aria-label' | 'aria-labelledby'> & {
  /** The accessible name. An icon alone names nothing, so the type will not compile without it. */
  readonly label: string;
  readonly icon: ReactNode;
};

export function IconButton({ label, icon, variant = 'ghost', size = 'md', loading = false, className, type = 'button', onClick, ref, ...rest }: IconButtonProps) {
  const waiting = loading || rest['aria-disabled'] === true || rest['aria-disabled'] === 'true';
  const press = (event: MouseEvent<HTMLButtonElement>) => {
    if (waiting) event.preventDefault();
    else onClick?.(event);
  };
  return (
    <button ref={ref} type={type} aria-label={label} title={label} data-ui="icon-button" data-variant={variant} data-size={size}
      data-loading={loading || undefined} {...rest} aria-busy={loading || undefined} aria-disabled={waiting || undefined}
      onClick={press} className={buttonClass(variant, size, true, className)}>
      {loading ? <LoaderCircle {...ICON} className="animate-spin" /> : icon}
    </button>
  );
}
