// SPDX-License-Identifier: MIT OR Apache-2.0

import { Check, Copy } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';

import { ticketAddress } from '../../navigation.ts';
import { Button, ICON, Input } from '../../ui/index.ts';
import { copyText } from './copyLink.ts';

const SAID_MS = 2000;

/**
 * Copies the address that opens this ticket. Where the browser refuses the clipboard the address is shown selected,
 * so it can still be copied by hand.
 */
export function CopyLink({ id }: { readonly id: string }) {
  const [state, setState] = useState<{ readonly kind: 'idle' | 'copied' } | { readonly kind: 'select'; readonly address: string }>({ kind: 'idle' });
  const field = useRef<HTMLInputElement>(null);
  useEffect(() => { setState({ kind: 'idle' }); }, [id]);
  useEffect(() => {
    if (state.kind === 'select') {
      field.current?.focus();
      field.current?.select();
    }
    if (state.kind !== 'copied') return undefined;
    const timer = setTimeout(() => setState({ kind: 'idle' }), SAID_MS);
    return () => clearTimeout(timer);
  }, [state]);

  function copy(): void {
    const address = ticketAddress(window.location, id);
    void copyText(navigator.clipboard, address).then((outcome) => {
      setState(outcome === 'copied' ? { kind: 'copied' } : { kind: 'select', address });
    });
  }

  return <span className="copy-link inline-flex flex-wrap items-center gap-2">
    <Button size="sm" className="ticket-copy-link" icon={state.kind === 'copied' ? <Check {...ICON} /> : <Copy {...ICON} />}
      onClick={copy}>{state.kind === 'copied' ? 'Copied' : 'Copy link'}</Button>
    {state.kind === 'select'
      ? <Input ref={field} readOnly className="ticket-link-field w-72 max-w-full" aria-label="Ticket link, selected to copy by hand"
        value={state.address} />
      : null}
    <span className="sr-only" role="status">{state.kind === 'copied' ? 'Link copied.' : state.kind === 'select' ? 'The link is selected. Copy it by hand.' : ''}</span>
  </span>;
}
