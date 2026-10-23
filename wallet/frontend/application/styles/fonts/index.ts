import localFont from "next/font/local";

// Self-hosted via next/font (no render-blocking <link> to Google Fonts).
// Exposes a CSS variable consumed by globals.css / the Tailwind theme.
// Inter is the only face the cabinet ships: it backs the sans body copy, the
// "mono-tech" labels (tracked-out, uppercase) and the headings that once used
// the Playfair display serif — one quieter, institutional grotesque throughout.
// Self-hosted (not next/font/google) so the production image builds
// hermetically — no Google fetch in the nix sandbox.
//
// One subset .woff2, the same bytes site_conductor ships (see README.md), not
// the upstream variable .ttf: next/font <link rel=preload>s every `src` entry
// on every page, and the two TTFs cost 1.4 MB gzipped per cold visit — a third
// of that for an italic face nothing in the cabinet uses. The upstream file
// name also carried a comma, which the preload emitted as `%2C` while the
// @font-face kept literal, so Chrome fetched the regular face twice.
export const fontInter = localFont({
  src: [
    {
      path: "./Inter-Variable.woff2",
      style: "normal",
      weight: "100 900",
    },
  ],
  display: "swap",
  variable: "--font-inter",
});
