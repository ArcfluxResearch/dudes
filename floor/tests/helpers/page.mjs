// SPDX-License-Identifier: MIT OR Apache-2.0

import { Fragment, createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

/**
 * The stage beside the office as the shell draws it: an open Control view, the
 * action receipts, the selected agent's drawer and the one Retire confirmation.
 */
export async function loadStage(server) {
  const { Control } = await server.ssrLoadModule('/src/renderer/Control.tsx');
  const { AgentDrawer, Receipts } = await server.ssrLoadModule('/src/renderer/AgentDrawer.tsx');
  const { RetireDialog } = await server.ssrLoadModule('/src/renderer/retire/RetirePanel.tsx');
  return ({ floor, row = floor.agents[0] ?? null, view = null, leaveFocusTo = 'crew-toggle' }) => renderToStaticMarkup(
    createElement(Fragment, null,
      view === null ? null : createElement(Control, {
        floor, view, selectedId: row?.id ?? null, onClose() {}, onView() {}, onOpenQuestion() {},
      }),
      createElement(Receipts, { floor, leaveFocusTo }),
      createElement(AgentDrawer, { floor, row, selectedId: row?.id ?? null, onClose() {}, leaveFocusTo }),
      createElement(RetireDialog, { floor })));
}
