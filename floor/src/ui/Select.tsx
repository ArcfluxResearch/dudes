// SPDX-License-Identifier: MIT OR Apache-2.0

import { useCallback, useId, useLayoutEffect, useRef, useState } from 'react';
import type { KeyboardEvent } from 'react';

import { cx } from './cx.ts';
import { useFieldControl } from './fieldContext.ts';
import { CheckIcon, ChevronDownIcon } from './icons.tsx';
import { useAnchoredPopover } from './popover.ts';
import './base.css';
import './popover.css';
import './Select.css';

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
  readonly name?: string;
  readonly disabled?: boolean;
  readonly autoFocus?: boolean;
  readonly className?: string;
  readonly 'aria-label'?: string;
  readonly 'aria-describedby'?: string;
  readonly 'aria-invalid'?: boolean;
}

const TYPE_AHEAD_MS = 600;

/**
 * A select-only combobox: a button that opens a listbox. Focus stays on the button and the listbox is named through
 * aria-activedescendant, so Escape and selection need no focus hand-back.
 */
export function Select({ options, value, onValueChange, id, name, disabled = false, autoFocus, className, ...aria }: SelectProps) {
  const auto = useId();
  const field = useFieldControl();
  const own = id ?? field.id ?? auto;
  const listId = `${own}-list`;
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const trigger = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const typed = useRef<{ text: string; at: number }>({ text: '', at: 0 });
  const selected = Math.max(0, options.findIndex((option) => option.value === value));
  const close = useCallback((returnFocus: boolean) => {
    setOpen(false);
    if (returnFocus) trigger.current?.focus();
  }, []);
  useAnchoredPopover(open, trigger, panel, close, 'start', true);

  useLayoutEffect(() => {
    if (open) panel.current?.querySelector('[data-active="true"]')?.scrollIntoView({ block: 'nearest' });
  }, [open, active]);

  function enabled(from: number, step: 1 | -1): number {
    for (let i = 1; i <= options.length; i += 1) {
      const at = (from + step * i + options.length * 2) % options.length;
      if (options[at]?.disabled !== true) return at;
    }
    return from;
  }

  function openAt(at: number): void {
    setActive(at);
    setOpen(true);
  }

  function pick(at: number): void {
    const option = options[at];
    if (option === undefined || option.disabled === true) return;
    if (option.value !== value) onValueChange(option.value);
    close(true);
  }

  function match(character: string): number {
    const now = Date.now();
    const text = now - typed.current.at > TYPE_AHEAD_MS ? character : typed.current.text + character;
    typed.current = { text, at: now };
    const from = text.length === 1 ? (open ? active : selected) + 1 : (open ? active : selected);
    for (let i = 0; i < options.length; i += 1) {
      const at = (from + i) % options.length;
      if (options[at]?.disabled !== true && options[at]?.label.toLowerCase().startsWith(text.toLowerCase())) return at;
    }
    return -1;
  }

  function onKeyDown(event: KeyboardEvent<HTMLButtonElement>): void {
    if (disabled || event.altKey || event.ctrlKey || event.metaKey) return;
    const { key } = event;
    if (key === 'Escape') {
      if (open) { event.preventDefault(); event.stopPropagation(); close(true); }
      return;
    }
    if (key === 'Tab') { if (open) close(false); return; }
    if (key.length === 1 && key !== ' ') {
      const at = match(key);
      if (at !== -1) { event.preventDefault(); if (open) setActive(at); else if (options[at]!.value !== value) onValueChange(options[at]!.value); }
      return;
    }
    if (key === ' ' && typed.current.text !== '' && Date.now() - typed.current.at < TYPE_AHEAD_MS) {
      event.preventDefault();
      const at = match(' ');
      if (at !== -1 && open) setActive(at);
      return;
    }
    if (!open) {
      if (key === 'ArrowDown' || key === 'ArrowUp' || key === 'Enter' || key === ' ' || key === 'Home' || key === 'End') {
        event.preventDefault();
        openAt(key === 'Home' ? enabled(-1, 1) : key === 'End' ? enabled(options.length, -1) : selected);
      }
      return;
    }
    if (key === 'ArrowDown') setActive(enabled(active, 1));
    else if (key === 'ArrowUp') setActive(enabled(active, -1));
    else if (key === 'Home') setActive(enabled(-1, 1));
    else if (key === 'End') setActive(enabled(options.length, -1));
    else if (key === 'Enter' || key === ' ') pick(active);
    else return;
    event.preventDefault();
  }

  const label = options[selected]?.label ?? '';
  return (
    <div className={cx(className, 'ui-select')}>
      <button ref={trigger} type="button" id={own} className="ui-select-trigger" role="combobox" name={name} disabled={disabled} autoFocus={autoFocus}
        aria-haspopup="listbox" aria-expanded={open} aria-controls={listId}
        aria-activedescendant={open ? `${own}-opt-${active}` : undefined} aria-label={aria['aria-label']}
        aria-describedby={aria['aria-describedby'] ?? field.describedBy} aria-invalid={aria['aria-invalid'] ?? field.invalid}
        onClick={() => { if (open) close(false); else openAt(selected); }} onKeyDown={onKeyDown}>
        <span className="ui-select-value">{label}</span>
        <ChevronDownIcon className="ui-select-chevron" />
      </button>
      <div ref={panel} className="ui-pop ui-select-panel" id={listId} role="listbox" popover="manual"
        aria-label={aria['aria-label']} tabIndex={-1}
        onMouseDown={(event) => event.preventDefault()}>
        {options.map((option, at) => (
          <div key={option.value} id={`${own}-opt-${at}`} role="option" className="ui-pop-item ui-select-option"
            aria-selected={option.value === value} aria-disabled={option.disabled || undefined}
            data-active={open && at === active ? 'true' : undefined}
            onMouseMove={() => { if (at !== active && option.disabled !== true) setActive(at); }}
            onClick={() => pick(at)}>
            <span className="ui-select-option-label">{option.label}</span>
            {option.value === value ? <CheckIcon className="ui-select-check" /> : null}
          </div>
        ))}
      </div>
    </div>
  );
}
