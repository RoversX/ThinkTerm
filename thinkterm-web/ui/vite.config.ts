import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// The page is served from whatever directory the mux server was pointed at,
// so every URL it emits has to be relative; pkg/ and fonts/ are written into
// the same directory by ci/build-web.sh and must survive the build.
export default defineConfig({
  plugins: [svelte()],
  base: './',
  publicDir: false,
  build: {
    outDir: '../www',
    emptyOutDir: false,
    target: 'esnext',
    cssCodeSplit: false,
    assetsDir: 'assets',
    sourcemap: false,
    modulePreload: { polyfill: false },
  },
});
