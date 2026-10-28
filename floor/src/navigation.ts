export type PageView = 'projects' | 'log' | 'setup' | 'settings';

const PAGE_HASH: Readonly<Record<PageView, string>> = Object.freeze({
  projects: '#/projects', log: '#/log', setup: '#/setup', settings: '#/settings',
});

export function pageFromHash(hash: string): PageView | null {
  switch (hash) {
    case '#/projects': return 'projects';
    case '#/log': return 'log';
    case '#/setup': return 'setup';
    case '#/settings': return 'settings';
    default: return null;
  }
}

export function isPageView(view: string): view is PageView {
  return view === 'projects' || view === 'log' || view === 'setup' || view === 'settings';
}

export function currentPage(): PageView | null {
  return typeof window === 'undefined' ? null : pageFromHash(window.location.hash);
}

export function subscribeToPage(listener: () => void): () => void {
  window.addEventListener('hashchange', listener);
  window.addEventListener('popstate', listener);
  return () => {
    window.removeEventListener('hashchange', listener);
    window.removeEventListener('popstate', listener);
  };
}

export function openPage(page: PageView | null): void {
  window.location.hash = page === null ? '' : PAGE_HASH[page];
}

const BOARD_LINK = /^#\/board(?:\/([a-zA-Z0-9_-]{1,128}))?$/;

/** The address that opens the Board on one project, such as `#/board/service`. */
export function boardLink(project: string): string {
  return `#/board/${project}`;
}

/**
 * What a Board address asks for. `#/board` opens the Board as it was and `#/board/<project>` opens it filtered to one
 * project. The Board is a panel, not a page, so this is not a page address: the shell reads it once and clears it.
 */
export function boardLinkFromAddress(address: string): { readonly project: string | null } | null {
  const match = BOARD_LINK.exec(address);
  return match === null ? null : { project: match[1] ?? null };
}

const TICKET_LINK = /^#\/ticket\/([a-zA-Z0-9_-]{1,128})$/;

/** The address that opens the Board on one ticket: `#/ticket/` and the ticket's id. */
export function ticketLink(id: string): string {
  return `#/ticket/${id}`;
}

/**
 * The ticket a `#/ticket/<id>` address names. Like a Board address it is read once by the shell and cleared, so it
 * never stays in the bar to be reloaded into a ticket the operator has since left.
 */
export function ticketFromAddress(address: string): string | null {
  return TICKET_LINK.exec(address)?.[1] ?? null;
}

/** The address to copy for a ticket. It is built from the origin and path alone, never from the sign-in fragment. */
export function ticketAddress(place: { readonly origin: string; readonly pathname: string }, id: string): string {
  return `${place.origin}${place.pathname}${ticketLink(id)}`;
}

/** Whether a signed-in tab may keep this address on load: a page, or a Board or ticket address the shell reads once. */
export function isOpenableAddress(address: string): boolean {
  return pageFromHash(address) !== null || boardLinkFromAddress(address) !== null || ticketFromAddress(address) !== null;
}

export function currentAddress(): string {
  return typeof window === 'undefined' ? '' : window.location.hash;
}
