// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';
import { clearNewTicketDraft, clearTicketChangesDraft, EMPTY_TICKET_DRAFT, readNewTicketDraft,
  readTicketChangesDraft, writeNewTicketDraft, writeTicketChangesDraft } from '../src/renderer/view.ts';

test('a late successful send cannot erase a newer ticket or changes draft', () => {
  const values = new Map();
  const storage = () => ({ getItem: (key) => values.get(key) ?? null, setItem: (key, value) => values.set(key, value), removeItem: (key) => values.delete(key) });
  const sent = { title: 'Submitted work', note: 'A note', project: null, check: null, agent: '', priority: 'none' };
  const newer = { ...sent, note: 'Keep this newer note' };
  writeNewTicketDraft(newer, storage);
  clearNewTicketDraft(sent, storage);
  assert.deepEqual(readNewTicketDraft(storage), newer);
  clearNewTicketDraft(newer, storage);
  assert.deepEqual(readNewTicketDraft(storage), EMPTY_TICKET_DRAFT);
  assert.equal(values.has('dudes.floor.new-ticket-draft'), false);
  writeTicketChangesDraft('one', 'Newer request', storage);
  writeTicketChangesDraft('two', 'Another ticket', storage);
  clearTicketChangesDraft('one', 'Submitted request', storage);
  assert.equal(readTicketChangesDraft('one', storage), 'Newer request');
  clearTicketChangesDraft('one', 'Newer request', storage);
  assert.equal(readTicketChangesDraft('one', storage), '');
  assert.equal(values.has('dudes.floor.ticket-changes-draft.one'), false);
  assert.equal(readTicketChangesDraft('two', storage), 'Another ticket');
});

test('malformed stored drafts and unavailable browser storage leave usable empty forms', () => {
  for (const raw of ['null', '{}', 'false', '{', JSON.stringify({ ...EMPTY_TICKET_DRAFT, title: 'a'.repeat(257) })]) {
    assert.deepEqual(readNewTicketDraft(() => ({ getItem: () => raw })), EMPTY_TICKET_DRAFT);
  }
  const unavailable = () => { throw Error('storage is unavailable'); };
  assert.deepEqual(readNewTicketDraft(unavailable), EMPTY_TICKET_DRAFT);
  assert.equal(readTicketChangesDraft('one', unavailable), '');
  assert.doesNotThrow(() => writeNewTicketDraft(EMPTY_TICKET_DRAFT, unavailable));
  assert.doesNotThrow(() => writeTicketChangesDraft('one', 'Keep working', unavailable));
  assert.doesNotThrow(() => clearNewTicketDraft(EMPTY_TICKET_DRAFT, unavailable));
  assert.doesNotThrow(() => clearTicketChangesDraft('one', '', unavailable));
  let removed = false;
  const unreadable = () => ({ getItem() { throw Error('unreadable'); }, removeItem() { removed = true; } });
  clearNewTicketDraft(EMPTY_TICKET_DRAFT, unreadable);
  clearTicketChangesDraft('one', '', unreadable);
  assert.equal(removed, false);
  const unremovable = () => ({ getItem: () => 'Sent', removeItem() { throw Error('unavailable'); } });
  assert.doesNotThrow(() => clearTicketChangesDraft('one', 'Sent', unremovable));
});

test('sent draft cleanup compares normalized fields before removing the key', () => {
  const values = new Map([['dudes.floor.new-ticket-draft', JSON.stringify({
    agent: '', check: null, project: null, note: 'Keep the details', title: 'Review the page', ignored: 'older field',
  })]]);
  const storage = () => ({ getItem: (key) => values.get(key) ?? null, removeItem: (key) => values.delete(key) });
  clearNewTicketDraft({ title: 'Review the page', note: 'Keep the details', project: null, check: null, agent: '', priority: 'none' }, storage);
  assert.equal(values.has('dudes.floor.new-ticket-draft'), false);
});

test('priority is part of the saved draft, and a stored draft without one reads as no priority', () => {
  const values = new Map();
  const storage = () => ({ getItem: (key) => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) });
  writeNewTicketDraft({ ...EMPTY_TICKET_DRAFT, title: 'Urgent fix', priority: 'urgent' }, storage);
  assert.equal(readNewTicketDraft(storage).priority, 'urgent');
  values.set('dudes.floor.new-ticket-draft', JSON.stringify({ title: 'Older draft', note: '', project: null, check: null, agent: '' }));
  assert.equal(readNewTicketDraft(storage).priority, 'none');
  values.set('dudes.floor.new-ticket-draft', JSON.stringify({ ...EMPTY_TICKET_DRAFT, title: 'Odd', priority: 'soon' }));
  assert.deepEqual(readNewTicketDraft(storage), EMPTY_TICKET_DRAFT);
});
