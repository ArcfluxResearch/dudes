import { RING } from '../../ui/cx.ts';
import { ACCENT_COLOURS, isAccentColour } from '../appearance.ts';

/** The accent colour radios shared by Hire and the look form: Automatic, then each colour with its swatch. */
export function AccentChoice({ name, value, onChange, disabled = false }: {
  readonly name: string;
  /** `null` is Automatic. */
  readonly value: string | null;
  readonly onChange: (accent: string | null) => void;
  readonly disabled?: boolean;
}) {
  const option = 'inline-flex cursor-pointer items-center gap-1.5 text-base text-ink';
  return <fieldset className="ui m-0 flex min-w-0 flex-wrap items-center gap-x-4 gap-y-2 border-0 p-0 font-sans" disabled={disabled}>
    <legend className="mb-1.5 p-0 text-small font-medium text-ink">Accent colour</legend>
    <label className={option}><input type="radio" name={name} value="" checked={value === null} className={`m-0 accent-accent ${RING}`}
      onChange={() => onChange(null)} />Automatic</label>
    {Object.keys(ACCENT_COLOURS).filter(isAccentColour).map((accent) => <label key={accent} className={option}>
      <input type="radio" name={name} value={accent} checked={value === accent} className={`m-0 accent-accent ${RING}`}
        onChange={() => onChange(accent)} />
      <span aria-hidden="true" className="inline-block size-3 rounded-pill border border-line-strong" style={{ backgroundColor: ACCENT_COLOURS[accent] }} />
      {accent[0]?.toUpperCase()}{accent.slice(1)}
    </label>)}
  </fieldset>;
}
