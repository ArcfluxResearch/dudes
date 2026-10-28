export type PageView = 'log' | 'setup' | 'settings';

const PAGE_HASH: Readonly<Record<PageView, string>> = Object.freeze({
  log: '#/log', setup: '#/setup', settings: '#/settings',
});

export function pageFromHash(hash: string): PageView | null {
  switch (hash) {
    case '#/log': return 'log';
    case '#/setup': return 'setup';
    case '#/settings': return 'settings';
    default: return null;
  }
}

export function isPageView(view: string): view is PageView {
  return view === 'log' || view === 'setup' || view === 'settings';
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

export function currentAddress(): string {
  return typeof window === 'undefined' ? '' : window.location.hash;
}
