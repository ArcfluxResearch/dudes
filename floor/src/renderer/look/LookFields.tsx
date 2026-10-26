// SPDX-License-Identifier: MIT OR Apache-2.0

import { ACCENT_COLOURS, CAST_CHARACTERS, isAccentColour } from '../appearance.ts';
import { RING, cn } from '../../ui/index.ts';

/** The characters a person can pick, with Automatic first, for a `Select`. */
export const CHARACTER_OPTIONS: readonly { readonly value: string; readonly label: string }[] = Object.freeze([
  { value: '', label: 'Automatic' },
  ...CAST_CHARACTERS.map((character) => ({ value: character, label: `Character ${character.toUpperCase()}` })),
]);

const PILL = cn('box-border inline-flex cursor-pointer items-center gap-2 rounded-pill border border-line bg-surface px-3 py-1',
  'text-base text-ink transition-colors hover:bg-surface-2 has-checked:border-ink has-checked:bg-surface-2',
  'has-disabled:cursor-not-allowed has-disabled:opacity-60');

const DOT = cn('m-0 box-border size-3.5 shrink-0 cursor-pointer appearance-none rounded-pill border border-line-strong',
  'disabled:cursor-not-allowed', RING);

/** The accent colour radios: Automatic, then each colour as a pill with its swatch. `null` is Automatic. */
export function AccentChoice({ name, value, onChange, disabled = false }: {
  readonly name: string;
  readonly value: string | null;
  readonly onChange: (accent: string | null) => void;
  readonly disabled?: boolean;
}) {
  return <fieldset disabled={disabled} className="m-0 flex min-w-0 flex-col gap-2 border-0 p-0 font-sans">
    <legend className="mb-2 p-0 text-small font-medium text-ink">Accent colour</legend>
    <div className="flex flex-wrap gap-2">
      <label data-ui="choice" data-type="radio" className={PILL}>
        <input type="radio" name={name} value="" checked={value === null} onChange={() => onChange(null)} className={cn(DOT, 'bg-surface-2')} />
        Automatic
      </label>
      {Object.keys(ACCENT_COLOURS).filter(isAccentColour).map((accent) => <label key={accent} data-ui="choice" data-type="radio" className={PILL}>
        <input type="radio" name={name} value={accent} checked={value === accent} onChange={() => onChange(accent)}
          className={DOT} style={{ backgroundColor: ACCENT_COLOURS[accent] }} />
        {accent[0]?.toUpperCase()}{accent.slice(1)}
      </label>)}
    </div>
  </fieldset>;
}
