// SPDX-License-Identifier: MIT OR Apache-2.0

import { X } from 'lucide-react';
import { useEffect, useRef } from 'react';

import { Dialog, ICON, IconButton } from '../../ui/index.ts';
import { shortcutRows } from './shortcuts.ts';

/** The Board's shortcuts, one row each, in a dialog that returns focus to where it was opened. */
export function ShortcutHelp({ mac, onClose }: { readonly mac: boolean; readonly onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const node = dialog.current;
    node?.showModal();
    return () => { node?.close(); };
  }, []);
  return <Dialog ref={dialog} size="sm" className="shortcut-help" aria-labelledby="shortcut-help-title"
    onCancel={(event) => { event.preventDefault(); onClose(); }}
    onKeyDown={(event) => { if (event.key === 'Escape') event.stopPropagation(); }}>
    <div className="flex flex-col gap-3 p-6">
      <header className="flex items-center justify-between gap-2">
        <h2 id="shortcut-help-title" className="m-0 text-title font-semibold">Keyboard shortcuts</h2>
        <IconButton label="Close shortcuts" icon={<X {...ICON} />} autoFocus onClick={onClose} />
      </header>
      <dl className="m-0 grid grid-cols-[auto_minmax(0,1fr)] items-center gap-x-4 gap-y-2 text-base">
        {shortcutRows(mac).map((row) => <div key={row.label} className="contents">
          <dt className="flex gap-1">{row.keys.map((key) => <kbd key={key}
            className="rounded-sm border border-line-strong bg-surface-2 px-1.5 py-0.5 font-mono text-small text-ink">{key}</kbd>)}</dt>
          <dd className="m-0 text-ink">{row.label}</dd>
        </div>)}
      </dl>
      <p className="m-0 text-small text-muted">Shortcuts are off while you type in a field.</p>
    </div>
  </Dialog>;
}
