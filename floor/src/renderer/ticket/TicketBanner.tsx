// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, useState } from 'react';

import './ticket.css';
import { useAutoDismiss } from '../useAutoDismiss.ts';

import type { FloorState } from '../floor/store.ts';
import { announcement, arrivals, bannerCounts, bannerText, viewConfirmed } from './model.ts';

/** The page title for a banner, or `base` alone when nothing waits. */
export function titleFor(base: string, text: string | null): string {
  return text === null ? base : `${text} ${base}`;
}

/**
 * The top bar's note that a ticket waits on the operator: Ready for you, in Needs you, or a Working
 * ticket's agent is asking. It is read from the cards of each snapshot and from nothing an agent
 * does, so a turn ending never raises it. A ticket is announced once, in a polite live region, when
 * it arrives in one of those states; a page opened with tickets already waiting announces none.
 * Dismissing the banner is local to the page and lasts until its words change.
 */
export function TicketBanner({ floor, onOpen }: { readonly floor: FloorState; readonly onOpen: () => void }) {
  const text = bannerText(bannerCounts(floor));
  const [dismissed, setDismissed] = useState<string | null>(null);
  const [said, setSaid] = useState('');
  const seen = useRef<ReadonlySet<string> | null>(null);
  const confirmed = viewConfirmed(floor);
  const base = useRef<string | null>(null);
  const banner = useRef<HTMLDivElement>(null);
  useAutoDismiss(banner, text === null || dismissed === text ? null : text, () => setDismissed(text));

  useEffect(() => {
    if (!confirmed) return;
    const found = arrivals(seen.current, floor.cards);
    seen.current = found.next;
    setSaid(found.announce.map(announcement).join(' '));
  }, [confirmed, floor.cards]);

  useEffect(() => {
    base.current ??= document.title;
    document.title = titleFor(base.current, text);
  }, [text]);

  return <>
    <div className="visually-hidden" role="status" aria-live="polite" aria-atomic="true" data-ticket-announcement="true">{said}</div>
    {text === null || dismissed === text ? null : <div className="ticket-banner" ref={banner} role="group" aria-label="Tickets waiting on you">
      <span className="ticket-banner-text">{text}</span>
      <button type="button" className="ticket-banner-open" onClick={onOpen}>Open Board</button>
      <button type="button" className="ticket-banner-dismiss" aria-label="Dismiss ticket banner" onClick={() => setDismissed(text)}>Dismiss</button>
    </div>}
  </>;
}
