import { createContext } from 'react';

/** How an item asks its menu to close and hand focus back to the button. */
export const MenuClose = createContext<() => void>(() => {});
