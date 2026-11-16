import { defineConfig } from 'vitest/config';

// Shared config; the node/jsdom split lives in `vitest.workspace.ts`, as in @evinvest/mfe.
export default defineConfig({
  esbuild: {
    jsx: 'automatic',
  },
});
