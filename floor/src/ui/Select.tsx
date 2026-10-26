import { Check, ChevronDown } from 'lucide-react';
import { useCallback, useEffect, useId, useRef, useState } from 'react';
import type { FocusEvent, KeyboardEvent, Ref } from 'react';

import { FIELD_BOX } from './Field.tsx';
import { useFieldControl } from './fieldContext.ts';
import { cn } from './cn.ts';
import { TYPEAHEAD_MS, isTypeaheadKey, moveActive, typeahead } from './listNav.ts';
import { ICON } from './look.ts';
import { useTopLayerPopover } from './popover.ts';

export interface SelectOption {
  readonly value: string;
  readonly label: string;
  readonly disabled?: boolean;
}

export interface SelectProps {
  readonly options: readonly SelectOption[];
  readonly value: string;
  readonly onValueChange: (value: string) => void;
  readonly id?: string;
  /** Named for the field it stands for, so a reader of the markup can tell which field it is. */
  readonly name?: string;
  readonly placeholder?: string;
  readonly disabled?: boolean;
  readonly autoFocus?: boolean;
  readonly className?: string;
  readonly ref?: Ref<HTMLButtonElement>;
  readonly 'aria-label'?: string;
  readonly 'aria-labelledby'?: string;
  readonly 'aria-describedby'?: string;
  readonly 'aria-invalid'?: true;
}

/**
 * A button that opens a listbox, for choosing one value. Focus stays on the button while the list is open, and the
 * active option is named by `aria-activedescendant`, so Escape, Tab and the arrow keys all leave focus where the person
 * was. The list is a child of the component, so it is safe inside a modal `dialog`.
 */
export function Select({ options, value, onValueChange, id, name, placeholder = 'Choose', disabled = false, autoFocus, className, ref, ...aria }: SelectProps) {
  const made = useId();
  const field = useFieldControl();
  const own = id ?? field.id ?? made;
  const listId = `${own}-list`;
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const trigger = useRef<HTMLButtonElement | null>(null);
  const list = useRef<HTMLDivElement | null>(null);
  const run = useRef({ text: '', at: 0 });
  const selected = options.findIndex((option) => option.value === value);
  const close = useCallback(() => setOpen(false), []);
  useTopLayerPopover(open, trigger, list, close, true);

  useEffect(() => {
    if (open) list.current?.querySelector('[data-active="true"]')?.scrollIntoView({ block: 'nearest' });
  }, [open, active]);

  function begin(at: number): void {
    setActive(at);
    setOpen(true);
  }

  function choose(at: number): void {
    const option = options[at];
    if (option === undefined || option.disabled === true) return;
    setOpen(false);
    if (option.value !== value) onValueChange(option.value);
  }

  function keyOf(event: KeyboardEvent<HTMLButtonElement>): void {
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    const now = Date.now();
    if (isTypeaheadKey(event) && !(event.key === ' ' && run.current.text === '')) {
      run.current = { text: now - run.current.at > TYPEAHEAD_MS ? event.key : run.current.text + event.key, at: now };
      const found = typeahead(options, open ? active : selected, run.current.text);
      event.preventDefault();
      if (found === null) return;
      if (open) setActive(found);
      else choose(found);
      return;
    }
    if (!open) {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp' || event.key === 'Enter' || event.key === ' ' || event.key === 'Home' || event.key === 'End') {
        event.preventDefault();
        begin(event.key === 'Home' || event.key === 'End' ? (moveActive(options, selected, event.key) ?? -1)
          : selected !== -1 ? selected : (moveActive(options, -1, event.key === 'ArrowUp' ? 'End' : 'Home') ?? -1));
      }
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      close();
    } else if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      choose(active);
    } else if (event.key === 'Tab') {
      close();
    } else {
      const next = moveActive(options, active, event.key);
      if (next !== null) {
        event.preventDefault();
        setActive(next);
      }
    }
  }

  function blurred(event: FocusEvent<HTMLButtonElement>): void {
    if (!list.current?.contains(event.relatedTarget as Node | null)) close();
  }

  return <>
    <button ref={(node) => { trigger.current = node; if (typeof ref === 'function') ref(node); else if (ref) ref.current = node; }}
      type="button" id={own} data-ui="select" data-field={name} data-open={open || undefined} role="combobox" aria-haspopup="listbox"
      aria-expanded={open} aria-controls={listId}
      aria-activedescendant={open && active >= 0 ? `${listId}-${active}` : undefined}
      aria-label={aria['aria-label']} aria-labelledby={aria['aria-labelledby']}
      aria-describedby={aria['aria-describedby'] ?? field.describedBy} aria-invalid={aria['aria-invalid'] ?? field.invalid}
      disabled={disabled} autoFocus={autoFocus}
      onClick={() => { if (open) close(); else begin(selected === -1 ? (moveActive(options, -1, 'Home') ?? -1) : selected); }}
      onKeyDown={keyOf}
      onBlur={blurred}
      className={cn(FIELD_BOX, 'flex h-8 cursor-pointer items-center justify-between gap-2 text-left', className)}>
      <span className={cn('truncate', selected === -1 && 'text-subtle')}>{selected === -1 ? placeholder : options[selected]!.label}</span>
      <ChevronDown {...ICON} className={cn('shrink-0 text-muted transition-transform', open && 'rotate-180')} />
    </button>
    <div ref={list} id={listId} role="listbox" popover="manual" tabIndex={-1} data-ui="select-list"
      aria-label={aria['aria-label']} aria-labelledby={aria['aria-labelledby']}
      onMouseDown={(event) => event.preventDefault()}
      className="fixed inset-auto m-0 box-border max-h-70 animate-pop overflow-y-auto rounded-lg border border-line bg-surface p-1 font-sans text-ink shadow-pop">
      {options.map((option, at) => <div key={option.value} id={`${listId}-${at}`} role="option" data-value={option.value}
        aria-selected={at === selected} aria-disabled={option.disabled === true || undefined} data-active={at === active}
        onPointerMove={() => { if (option.disabled !== true && at !== active) setActive(at); }}
        onClick={() => choose(at)}
        className={cn('flex h-8 cursor-pointer items-center justify-between gap-2 rounded-md px-2 text-base max-[800px]:min-h-11',
          at === active && 'bg-surface-2', at === selected && 'font-medium', option.disabled === true && 'cursor-not-allowed opacity-50')}>
        <span className="truncate">{option.label}</span>
        {at === selected ? <Check {...ICON} className="shrink-0 text-accent" /> : null}
      </div>)}
    </div>
  </>;
}
