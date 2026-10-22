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
