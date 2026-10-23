"use client";

import { cn } from "@/shared/lib/cn";
import { NetworkMark } from "@/shared/ui/icons/networks";
import { networkLabel } from "@/views/wallet/lib/format";

// The full-width segmented rail picker (Figma `seg-*`): equal-width segments on a sunken
// surface track, the selection carrying the teal fill. Rails come from the wallet response,
// so the segment count varies — hence flex-1 segments rather than a fixed grid.
// Hand-written — uikit has no equivalent, so the segments carry their own focus ring.
export function NetworkSegments({ networks, value, onChange, label }: { networks: string[]; value: string; onChange: (network: string) => void; label: string }) {
  return (
    <div role="radiogroup" aria-label={label} className="flex w-full rounded-lg bg-secondary p-1">
      {networks.map((network) => {
        const selected = network === value;
        return (
          <button
            key={network}
            type="button"
            role="radio"
            aria-checked={selected}
            onClick={() => onChange(network)}
            className={cn(
              // `truncate` moved onto the label span when the mark arrived: the segments are
              // equal fractions of the row, so without it an over-long rail name spills out
              // of its segment rather than ellipsing, and a flex parent cannot truncate its
              // own children. The rail names on offer today are network codes and never do,
              // but nothing here guarantees the next one won't.
              "flex min-w-0 flex-1 items-center justify-center gap-1.5 rounded-md py-2 text-xs font-medium outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring lg:py-2.5 lg:text-sm",
              selected ? "bg-primary text-on-primary" : "text-ink-soft hover:text-ink",
            )}
          >
            {/* The name stays visible rather than being replaced by the mark: this is the
                accessible name of a `role="radio"`, and a chain's logo is not a word a
                screen reader can say. The mark is `aria-hidden` decoration on top of it. */}
            <NetworkMark network={network} className="size-3.5 shrink-0 lg:size-4" />
            <span className="truncate">{networkLabel(network)}</span>
          </button>
        );
      })}
    </div>
  );
}
