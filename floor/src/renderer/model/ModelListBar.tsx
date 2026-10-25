// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useSyncExternalStore } from 'react';

import { getFloorState, subscribeToFloor } from '../floor/store.ts';
import { RelativeTime } from '../RelativeTime.tsx';
import './model-list.css';
import { LISTS_REASON_WORDS, browserListsDeps, getListsState, heldList, listNote, refreshModels, subscribeToLists } from './lists.ts';

/** What the model list is, when it was made, and a way to ask the engine again. */
export function ModelListBar({ provider, id }: { readonly provider: string; readonly id: string }) {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const lists = useSyncExternalStore(subscribeToLists, getListsState, getListsState);
  const held = heldList(floor, provider);
  const mine = lists.provider === provider;
  const live = floor.control && floor.phase === 'live' && !floor.stale;
  useEffect(() => {
    if (provider !== 'claude' && held === undefined && live) void refreshModels(provider, browserListsDeps, getFloorState(), false);
  }, [provider, held, live]);
  return <div className="model-list-bar" id={id} data-model-list={held === undefined ? 'none' : 'held'}>
    <small>{listNote(provider, held !== undefined)}{held === undefined ? null : <> Made <RelativeTime at={held.asOf} />.</>}</small>
    {live ? <button type="button" disabled={lists.sending} onClick={() => { void refreshModels(provider, browserListsDeps, getFloorState()); }}>
      Refresh list</button> : null}
    {mine && lists.sending ? <small role="status">Asking the engine…</small> : null}
    {mine && lists.outcome === 'refused' && lists.reason !== null ? <small role="status">{LISTS_REASON_WORDS[lists.reason]}</small> : null}
    {mine && (lists.outcome === 'unavailable' || lists.outcome === 'unknown') ? <small role="status">Dudes could not confirm the list was asked for. Try again.</small> : null}
    {mine && lists.outcome === 'pending' ? <small role="status">The engine is already being asked. Refresh the list again in a moment.</small> : null}
    {mine && lists.outcome === 'forbidden' ? <small role="status">This Floor session cannot refresh the list. Open a fresh session.</small> : null}
  </div>;
}
