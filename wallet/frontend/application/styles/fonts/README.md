# Fonts

One subset `.woff2` face, loaded by [`./index.ts`](./index.ts) through
`next/font/local` and committed as real bytes (no LFS: `next build` reads it and
the production image builds hermetically under Nix).

`Inter-Variable.woff2` is byte-identical to
`site_conductor/frontend/application/styles/fonts/Inter-Variable.woff2` and is
regenerated there — the recipe, the coverage (Latin-1, Latin Extended, Vietnamese,
Greek, Cyrillic, punctuation, currency, arrows, math, shapes, dingbats) and the
reasoning live in that folder's README. Copy the file over after regenerating it
there; do not subset it separately here, or the two apps drift apart.

No italic face: nothing in the cabinet or in `@evinvest/uikit` sets
`font-style: italic`, and next/font would preload it on every page regardless.
