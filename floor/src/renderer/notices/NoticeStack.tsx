// SPDX-License-Identifier: MIT OR Apache-2.0

import { useCallback, useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';

import { clockTime } from '../format.ts';
import { planStack, remember, severityOf } from './stack.ts';
import type { HistoryEntry } from './stack.ts';
import { Button, RING, cn } from '../../ui/index.ts';

/** Containers that stay in the list so a screen reader knows their live region; a notice is what they hold. */
const HOLDERS = '.done-notices, .device-notices';
/** Lines that come and go with the connection and are not worth keeping. */
const UNKEPT = '.stream-notice, .sr-only';

function noticeText(node: Node): string {
  if (node.nodeType === Node.TEXT_NODE) return node.textContent ?? '';
  if (node instanceof Element && node.matches('button, a')) return '';
  return [...node.childNodes].map(noticeText).join('');
}

function isHolder(node: Element): boolean {
  return node.matches(HOLDERS);
}

function shown(node: Element): boolean {
  return !node.matches('.sr-only') && (!isHolder(node) || (node.textContent ?? '').trim() !== '');
}

/**
 * The floating stack. Its children are the notices, each drawn by the component that owns it, and the stack
 * orders them by severity, folds all but a few away behind a count, and keeps what went by in a short
 * history. It takes no room from the page: it floats over the corner of the stage, away from the agent panel.
 */
export function NoticeStack({ host, children }: {
  readonly host: (list: HTMLDivElement | null) => void;
  readonly children: ReactNode;
}) {
  const list = useRef<HTMLDivElement | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [folded, setFolded] = useState(0);
  const [history, setHistory] = useState<readonly HistoryEntry[]>([]);
  const [reading, setReading] = useState(false);
  const attach = useCallback((node: HTMLDivElement | null) => { list.current = node; host(node); }, [host]);

  useEffect(() => {
    const element = list.current;
    if (element === null) return undefined;
    const arrange = () => {
      const notices = [...element.children].filter(shown);
      const plan = planStack(notices.map((node) => severityOf([...node.classList], node.getAttribute('data-tone'))), expanded);
      notices.forEach((node, at) => {
        (node as HTMLElement).style.order = String(plan.place[at]);
        node.toggleAttribute('data-folded', plan.folded[at]);
      });
      setFolded(plan.folded.filter(Boolean).length);
    };
    const observer = new MutationObserver((records) => {
      const gone: string[] = [];
      for (const record of records) {
        for (const node of record.removedNodes) {
          if (!(node instanceof Element)) continue;
          const units = record.target === element ? (isHolder(node) ? [] : [node])
            : node.matches('li') ? [node] : [...node.querySelectorAll('li')];
          for (const unit of units) if (!unit.matches(UNKEPT)) gone.push(noticeText(unit));
        }
      }
      if (gone.length > 0) setHistory((current) => gone.reduce((all, text) => remember(all, text, Date.now()), current));
      arrange();
    });
    observer.observe(element, { childList: true, subtree: true, characterData: true });
    arrange();
    return () => observer.disconnect();
  }, [expanded]);

  return (
    <div className="notice-stack flex min-h-0 flex-col gap-2" data-expanded={expanded ? 'true' : 'false'}>
      <div className={cn('notice-list flex min-h-0 flex-col gap-2 overflow-y-auto overscroll-contain rounded-lg [&>*]:flex-none [&>*]:max-w-full',
        '[&>[data-folded]]:hidden [&>.done-notices:empty]:absolute [&>.device-notices:empty]:absolute', RING)}
        ref={attach} id="floor-notices" role="region" aria-label="Floor notices" tabIndex={0}>
        {children}
      </div>
      {folded > 0 || expanded || history.length > 0 ? (
        <div className="notice-tools pointer-events-auto flex gap-2">
          {folded > 0 || expanded ? (
            <Button size="sm" aria-expanded={expanded} onClick={() => setExpanded((open) => !open)}>
              {expanded ? 'Show fewer' : `+${folded} more`}
            </Button>
          ) : null}
          {history.length > 0 ? (
            <Button size="sm" aria-expanded={reading} aria-controls="notice-history"
              onClick={() => setReading((open) => !open)}>History</Button>
          ) : null}
        </div>
      ) : null}
      {reading && history.length > 0 ? (
        <section className="notice-history pointer-events-auto grid max-h-56 gap-3 overflow-y-auto rounded-lg border border-line bg-surface p-3 text-small text-ink shadow-pop"
          id="notice-history" aria-label="Earlier notices">
          <ol className="m-0 grid list-none gap-2 p-0">
            {history.map((entry) => (
              <li key={entry.key}><time className="whitespace-nowrap text-muted">{clockTime(entry.at)}</time> {entry.text}</li>
            ))}
          </ol>
          <Button size="sm" className="justify-self-start" onClick={() => { setHistory([]); setReading(false); }}>Clear</Button>
        </section>
      ) : null}
    </div>
  );
}
