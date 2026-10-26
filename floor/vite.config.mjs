import { defineConfig } from 'vite';
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { daemonVersion } from './scripts/write-release-manifest.mjs';

export default defineConfig({
  base: '/floor/',
  define: { __FLOOR_BUILD_VERSION__: JSON.stringify(daemonVersion()) },
  publicDir: false,
  plugins: [react(), tailwindcss()],
  build: {
    assetsInlineLimit: 0,
    license: { fileName: '.vite/third-party-licenses.json' },
  },
});
