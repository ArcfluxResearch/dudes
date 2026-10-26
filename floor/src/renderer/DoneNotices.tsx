// SPDX-License-Identifier: MIT OR Apache-2.0

import { useRef } from 'react';

import type { DoneNotice } from './done.ts';
import { dismissDoneNotice } from './floor/store.ts';
import { handFocusAfterLeave } from './focus.ts';
import { useAutoDismiss } from './useAutoDismiss.ts';
import { Button } from '../ui/index.ts';
import { noticeCard } from './notices/look.tsx';

function dismissId(notice: DoneNotice): string {
  return `done-dismiss-${notice.key}`;
}

function DoneNoticeItem({ notice, focusNext }: { readonly notice: DoneNotice; readonly focusNext: string }) {
  const item = useRef<HTMLLIElement>(null);
  useAutoDismiss(item, notice.key, () => dismissDoneNotice(notice.key));
  return (
    <li ref={item} className={noticeCard('ok', 'done-notice flex flex-wrap items-center gap-x-3 gap-y-1 not-last:hidden')} data-done-agent-id={notice.agentId}>
      <p className="m-0 text-caption font-semibold uppercase tracking-label text-ok">Done</p>
      <p className="done-notice-met m-0"><strong className="font-semibold">{notice.name}</strong> <span>met its goal.</span></p>
      <p className="done-notice-goal m-0 min-w-0 flex-[1_1_128px] truncate text-muted" title={notice.goal}>{notice.goal}</p>
      <Button variant="ghost" size="sm" id={dismissId(notice)} ref={handFocusAfterLeave}
        data-focus-on-leave={focusNext}
        aria-label={`Dismiss the Done notice for ${notice.name}: ${notice.goal}`}
        onClick={() => dismissDoneNotice(notice.key)}>Dismiss</Button>
    </li>
  );
}

/**
 * The region is drawn even when empty, because a screen reader reads out only
 * what is added to a live region it already knows.
 */
export function DoneNotices({ notices, leaveFocusTo }: {
  readonly notices: readonly DoneNotice[];
  readonly leaveFocusTo: string;
}) {
  const more = notices.length - 1;
  return (
    <div className="done-notices pointer-events-none flex flex-col gap-2" aria-live="polite">
      {notices.length === 0 ? null : (
        <ol className="m-0 grid list-none gap-2 p-0" aria-label="Goals done">
          {notices.map((notice, at) => {
            const neighbour = notices[at + 1] ?? notices[at - 1];
            return <DoneNoticeItem key={notice.key} notice={notice}
              focusNext={neighbour === undefined ? leaveFocusTo : dismissId(neighbour)} />;
          })}
        </ol>
      )}
      {more > 0 ? <p className="done-notices-more pointer-events-auto m-0 w-fit rounded-lg bg-surface px-2 py-0.5 text-small text-muted"><span aria-hidden="true">{`+${more} more`}</span>
        <span className="done-notices-more-label sr-only">{`${more} more ${more === 1 ? 'goal' : 'goals'} done`}</span></p> : null}
    </div>
  );
}
