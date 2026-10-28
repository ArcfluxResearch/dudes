// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A request from outside the Board, such as the command menu, to open its new-ticket form with a title typed.

import { useEffect, useSyncExternalStore } from 'react';

interface NewTicketRequest {
  readonly title: string;
}

let requested: NewTicketRequest | null = null;
const listeners = new Set<() => void>();

export function requestNewTicket(title: string): void {
  requested = { title };
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

function current(): NewTicketRequest | null {
  return requested;
}

/** Open the new-ticket form for a request, once, if tickets can be made now. A request that cannot be met is dropped. */
export function useRequestedNewTicket(canCreate: boolean, onNew: (title: string) => void): void {
  const request = useSyncExternalStore(subscribe, current, current);
  useEffect(() => {
    if (request === null) return;
    requested = null;
    for (const listener of listeners) listener();
    if (canCreate) onNew(request.title);
  }, [request, canCreate, onNew]);
}
