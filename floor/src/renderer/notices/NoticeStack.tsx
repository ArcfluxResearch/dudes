// SPDX-License-Identifier: MIT OR Apache-2.0

import { useCallback, useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';

import { clockTime } from '../format.ts';
import { planStack, remember, severityOf } from './stack.ts';
import type { HistoryEntry } from './stack.ts';

/** Containers that stay in the list so a screen reader knows their live region; a notice is what they hold. */
const HOLDERS = '.done-notices, .device-notices';
/** Lines that come and go with the connection and are not worth keeping. */
const UNKEPT = '.stream-notice, .visually-hidden';

function noticeText(node: Node): string {
  if (node.nodeType === Node.TEXT_NODE) return node.textContent ?? '';
  if (node instanceof Element && node.matches('button, a')) return '';
  return [...node.childNodes].map(noticeText).join('');
}

function isHolder(node: Element): boolean {
  return node.matches(HOLDERS);
}

function shown(node: Element): boolean {
  return !node.matches('.visually-hidden') && (!isHolder(node) || (node.textContent ?? '').trim() !== '');
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
    <div className="notice-stack" data-expanded={expanded ? 'true' : 'false'}>
      <div className="notice-list" ref={attach} id="floor-notices" role="region"
        aria-label="Floor notices" tabIndex={0}>
        {children}
      </div>
      {folded > 0 || expanded || history.length > 0 ? (
        <div className="notice-tools">
          {folded > 0 || expanded ? (
            <button type="button" aria-expanded={expanded} onClick={() => setExpanded((open) => !open)}>
              {expanded ? 'Show fewer' : `+${folded} more`}
            </button>
          ) : null}
          {history.length > 0 ? (
            <button type="button" aria-expanded={reading} aria-controls="notice-history"
              onClick={() => setReading((open) => !open)}>History</button>
          ) : null}
        </div>
      ) : null}
      {reading && history.length > 0 ? (
        <section className="notice-history" id="notice-history" aria-label="Earlier notices">
          <ol>
            {history.map((entry) => (
              <li key={entry.key}><time>{clockTime(entry.at)}</time> {entry.text}</li>
            ))}
          </ol>
          <button type="button" onClick={() => { setHistory([]); setReading(false); }}>Clear</button>
        </section>
      ) : null}
    </div>
  );
}
