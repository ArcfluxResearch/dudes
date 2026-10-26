import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { resolve } from 'node:path';
import { defineConfig } from 'vite';

// The component lab: one page that draws every owned component, built only for the browser checks and never part of the
// release build, which starts from index.html.
export default defineConfig({
  root: resolve(import.meta.dirname, 'lab'),
  base: '/',
  publicDir: false,
  plugins: [react(), tailwindcss()],
  build: { assetsInlineLimit: 0, emptyOutDir: true },
});
