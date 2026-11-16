import { defineWorkspace } from 'vitest/config';

// Split by file suffix, as in @evinvest/mfe: `*.node.test.ts` runs in node (the
// server-safe core), `*.dom.test.ts` and `*.react.test.tsx` in jsdom.
export default defineWorkspace([
  {
    extends: './vitest.config.ts',
    test: {
      name: 'node',
      environment: 'node',
      include: ['test/**/*.node.test.ts'],
    },
  },
  {
    extends: './vitest.config.ts',
    test: {
      name: 'dom',
      environment: 'jsdom',
      setupFiles: ['./test/setup.dom.ts'],
      include: ['test/**/*.dom.test.ts', 'test/**/*.react.test.tsx'],
    },
  },
]);
