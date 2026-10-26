// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, useState } from 'react';
import type { KeyboardEvent, PointerEvent } from 'react';
import { GripVertical } from 'lucide-react';
import { cn, ICON, IconButton } from '../ui/index.ts';
import { agentOrder, moveAgent, readAgentOrder, reorderKeyTarget, writeAgentOrder } from './view.ts';
import type { AgentOrderView } from './view.ts';

export function useAgentOrder<T extends { readonly id: string }>(rows: readonly T[], view: AgentOrderView) {
  const [order, setOrder] = useState<readonly string[]>([]);
  useEffect(() => { setOrder(readAgentOrder(view)); }, [view]);
  const ordered = agentOrder(rows, order);
  function move(agent: string, target: string): void {
    const next = moveAgent(ordered.map((row) => row.id), agent, target);
    setOrder(next);
    writeAgentOrder(view, next);
  }
  return { ordered, move };
}

export function ReorderHandle({ agent, name, ids, scope, onMove }: {
  readonly agent: string;
  readonly name: string;
  readonly ids: readonly string[];
  readonly scope: string;
  readonly onMove: (agent: string, target: string) => void;
}) {
  const drag = useRef<{ pointer: number; target: string } | null>(null);
  const [dragging, setDragging] = useState(false);
  const position = ids.indexOf(agent) + 1;

  function startDrag(event: PointerEvent<HTMLButtonElement>): void {
    if (event.button !== 0 || !event.isPrimary) return;
    event.preventDefault();
    event.currentTarget.focus();
    event.currentTarget.setPointerCapture(event.pointerId);
    drag.current = { pointer: event.pointerId, target: agent };
    setDragging(true);
  }

  function moveDrag(event: PointerEvent<HTMLButtonElement>): void {
    if (drag.current?.pointer !== event.pointerId) return;
    const entry = event.currentTarget.ownerDocument.elementFromPoint(event.clientX, event.clientY)
      ?.closest<HTMLElement>('[data-reorder-agent]');
    const target = entry?.dataset.reorderAgent;
    if (entry?.dataset.reorderScope === scope && target !== undefined && ids.includes(target)) {
      drag.current.target = target;
    } else {
      drag.current.target = agent;
    }
  }

  function endDrag(event: PointerEvent<HTMLButtonElement>): void {
    const current = drag.current;
    if (current?.pointer !== event.pointerId) return;
    drag.current = null;
    setDragging(false);
    if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId);
    if (event.type === 'pointerup' && ids.includes(current.target)) onMove(agent, current.target);
  }

  function keyMove(event: KeyboardEvent<HTMLButtonElement>): void {
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const target = reorderKeyTarget(ids, agent, event.key);
    if (target === null) return;
    event.preventDefault();
    onMove(agent, target);
  }

  return <IconButton className={cn('agent-reorder-handle h-9 min-h-0 w-5 max-[800px]:h-11 max-[800px]:w-11 shrink-0 [&>svg]:shrink-0 cursor-grab touch-none text-subtle',
    'data-[dragging=true]:cursor-grabbing data-[dragging=true]:bg-surface-2 data-[dragging=true]:text-ink')}
  data-dragging={dragging ? 'true' : 'false'}
  label={`Reorder ${name}, position ${position} of ${ids.length}`}
  title="Drag to reorder, or use arrow keys, Home and End"
  disabled={ids.length < 2}
  onPointerDown={startDrag} onPointerMove={moveDrag} onPointerUp={endDrag}
  onPointerCancel={endDrag} onLostPointerCapture={endDrag} onKeyDown={keyMove}
  icon={<><GripVertical {...ICON} aria-hidden="true" /><span className="reorder-position sr-only" role="status">{position} of {ids.length}</span></>} />;
}
