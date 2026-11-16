import type { Plugin } from 'esbuild';
import { defineConfig } from 'tsup';

/**
 * The React subpath imports this package's core as `../index`; kept external
 * and pointed at the sibling `index.js` so the constants and parsers exist once.
 */
const coreAsSibling: Plugin = {
  name: 'core-as-sibling',
  setup(build) {
    build.onResolve({ filter: /^\.\.\/index$/ }, () => ({ path: './index.js', external: true }));
  },
};

// `@evinvest/mfe` stays external (a peer): its loader caches loads per tag, and
// a bundled second copy would load every element twice.
export default defineConfig({
  entry: { react: 'src/react/index.ts' },
  format: ['esm'],
  dts: true,
  clean: false,
  sourcemap: true,
  target: 'es2022',
  external: ['react', 'react-dom', 'react/jsx-runtime', '@evinvest/mfe', '@evinvest/mfe/react'],
  banner: { js: '"use client";' },
  esbuildPlugins: [coreAsSibling],
});
