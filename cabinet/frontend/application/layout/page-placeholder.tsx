import type { ReactNode } from "react";

// A styled, navigable "not built yet" surface for routes whose Figma screens are designed
// but not yet implemented (Operations · Settings). Keeps the shell coherent — these are
// real nav destinations, not dead links or 404s — until their views land.
export function PagePlaceholder({ eyebrow, title, blurb, icon }: { eyebrow: string; title: string; blurb: string; icon: ReactNode }) {
  return (
    <div className="px-8 pb-7 pt-6">
      <header className="mb-6 space-y-1">
        <p className="font-mono-tech text-xs uppercase tracking-widest text-accent-debug">{eyebrow}</p>
        <h1 className="text-2xl font-semibold text-ink">{title}</h1>
      </header>
      <div className="flex flex-col items-center gap-3 rounded-xl border border-border bg-card px-8 py-20 text-center">
        <span className="flex size-12 items-center justify-center rounded-full bg-accent-debug/10 text-accent-debug">{icon}</span>
        <p className="max-w-md text-sm text-ink-soft">{blurb}</p>
      </div>
    </div>
  );
}
