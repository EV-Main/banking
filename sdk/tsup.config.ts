import { defineConfig } from 'tsup';

// The server-safe core only; the React subpath is `tsup.react.config.ts`, run
// after this one by `build`/`prepare`, as in @evinvest/mfe: a `"use client"`
// banner must not reach the core, and tsup's `.d.ts` step deletes a concurrent
// config's declarations when this one cleans `dist/`.
export default defineConfig({
  entry: { index: 'src/index.ts' },
  format: ['esm'],
  dts: true,
  clean: true,
  sourcemap: true,
  target: 'es2022',
  external: ['@evinvest/mfe'],
});
