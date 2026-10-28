// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the command menu offers for what has been typed. None of it assigns a ticket or starts work: New ticket
// opens the same form as the Board's button, and a ticket opens its detail.

export interface TicketChoice {
  readonly id: string;
  readonly title: string;
  readonly detail: string;
}

export type MenuRow =
  | { readonly kind: 'new'; readonly id: 'new'; readonly label: string; readonly title: string }
  | { readonly kind: 'board' | 'list'; readonly id: 'board' | 'list'; readonly label: string }
  | { readonly kind: 'ticket'; readonly id: string; readonly label: string; readonly detail: string };

const TITLE_LIMIT = 256;
const TICKETS_SHOWN = 8;
const NEW_TICKET = 'new ticket';

const tokensOf = (query: string): readonly string[] => query.toLowerCase().split(/\s+/).filter((token) => token !== '');
const matches = (text: string, tokens: readonly string[]): boolean => {
  const low = text.toLowerCase();
  return tokens.every((token) => low.includes(token));
};

/**
 * The rows for `query`, in the order they are read: New ticket, the two views, then the tickets whose titles hold
 * every word typed, titles that start with the query first. New ticket carries the typed words as the title unless
 * they only spell out the command. It is left out when no ticket can be made.
 */
export function listCommands(query: string, tickets: readonly TicketChoice[], canCreate: boolean): readonly MenuRow[] {
  const typed = query.trim();
  const tokens = tokensOf(typed);
  const rows: MenuRow[] = [];
  if (canCreate) {
    const spelled = typed === '' || NEW_TICKET.startsWith(typed.toLowerCase());
    rows.push(spelled ? { kind: 'new', id: 'new', label: 'New ticket', title: '' }
      : { kind: 'new', id: 'new', label: `New ticket: ${typed}`, title: typed.slice(0, TITLE_LIMIT) });
  }
  if (matches('Open Board', tokens)) rows.push({ kind: 'board', id: 'board', label: 'Open Board' });
  if (matches('Open List', tokens)) rows.push({ kind: 'list', id: 'list', label: 'Open List' });
  const found = tickets.filter((ticket) => matches(ticket.title, tokens));
  const starts = typed.toLowerCase();
  const ranked = [...found.filter((ticket) => ticket.title.toLowerCase().startsWith(starts)),
    ...found.filter((ticket) => !ticket.title.toLowerCase().startsWith(starts))];
  for (const ticket of ranked.slice(0, TICKETS_SHOWN)) {
    rows.push({ kind: 'ticket', id: ticket.id, label: ticket.title, detail: ticket.detail });
  }
  return rows;
}
