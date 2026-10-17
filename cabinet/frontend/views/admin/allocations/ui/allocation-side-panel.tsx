"use client";

// The slot beside the registry table, and which panel fills it. Same collapse-width
// idiom as the Users screen's row drawer — see `UserDrawer` in
// `views/admin/users/ui/users-view.tsx` for why the width is fixed rather than `w-full`,
// and why the presence boundary lives at this level rather than inside each panel.
//
// Below `lg` there is no room beside the table for a 340px column, so the same panel is
// presented over the content as a bottom sheet instead — the operations timeline makes
// the identical Popover → Drawer trade at the same breakpoint.

import { Drawer, DrawerContent, DrawerTitle } from "@evinvest/uikit";

import { tenantOf } from "@/entities/grant/lib/access";
import { PanelAccessCard } from "@/features/panel-access";
import { useIsCompact } from "@/shared/lib/use-is-compact";
import { Panel, PanelPresence, PanelSwap } from "@/shared/ui/motion-panel";
import type { OpenAllocationPanel } from "@/views/admin/allocations/lib/panel";
import { BookPanel } from "@/views/admin/allocations/ui/book-panel";
import { GrantsPanel } from "@/views/admin/allocations/ui/grants-panel";
import { IssuancePanel } from "@/views/admin/allocations/ui/issuance-panel";
import { PanelHeader } from "@/views/admin/allocations/ui/panel-header";

// Inside the sheet the panel is the sheet: full width, and the card's own frame would be
// a border inside a border.
const SHEET_PANEL = "w-full rounded-none border-0 shadow-none";

function PanelBody({ panel, onClose, className }: { panel: OpenAllocationPanel; onClose: () => void; className?: string }) {
  switch (panel.kind) {
    case "grants":
      return <GrantsPanel key={panel.row.service} allocation={panel.row} onClose={onClose} className={className} />;
    case "issue":
      return <IssuancePanel key={panel.row.service} allocation={panel.row} onClose={onClose} className={className} />;
    case "book":
      return <BookPanel key={panel.row.service} allocation={panel.row} onClose={onClose} className={className} />;
    case "panelAccess": {
      const namespace = tenantOf(panel.row.service);
      if (namespace === null) throw new Error(`panel access opened on ${panel.row.service}, which has no tenant`);
      return <PanelAccessCard key={panel.row.service} namespace={namespace} header={<PanelHeader allocation={panel.row} onClose={onClose} />} className={className} />;
    }
  }
}

export function AllocationSidePanel({ panel, onClose }: { panel: OpenAllocationPanel | null; onClose: () => void }) {
  const compact = useIsCompact("lg");

  if (compact) {
    return (
      <Drawer open={panel !== null} onOpenChange={(open) => !open && onClose()}>
        {/* The kit animates the sheet and scrolls its body (uikit ≥ 0.16); what it does
            not do is cap it. A long holders roster has to scroll inside the sheet, not
            push it off-screen — the cap goes on the panel, never `overflow` (see the
            operations timeline for why). */}
        <DrawerContent className="max-h-[85vh]">
          {panel && (
            <>
              <DrawerTitle className="sr-only">{panel.row.title}</DrawerTitle>
              <PanelBody panel={panel} onClose={onClose} className={SHEET_PANEL} />
            </>
          )}
        </DrawerContent>
      </Drawer>
    );
  }

  return (
    <PanelPresence>
      {panel && (
        // `resize={false}`: each panel renders a skeleton until its own query lands, and
        // that data arrives without re-rendering this component. The size projection
        // measured the skeleton, animated towards it, then overshot when the real
        // content replaced it mid-flight. The body cross-fade still plays.
        <Panel key="side-panel" resize={false} collapse={{ gap: "1.5rem", width: "21.25rem" }} className="shrink-0 self-start overflow-hidden">
          {/* Keyed on kind AND row: switching from grants to issuance on the same row is a
              swap too, not an in-place re-render of a panel that is no longer there. */}
          <PanelSwap swapKey={`${panel.kind}:${panel.row.service}`}>
            <PanelBody panel={panel} onClose={onClose} />
          </PanelSwap>
        </Panel>
      )}
    </PanelPresence>
  );
}
