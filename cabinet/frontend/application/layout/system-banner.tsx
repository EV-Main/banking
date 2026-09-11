"use client";

import { TriangleAlert, X } from "lucide-react";
import { useState } from "react";

import { useT } from "@evinvest/i18n/react";

import { usePlatform } from "@/shared/lib/use-platform";

const DISMISS_KEY = "ev-announcement-dismissed";

function storedDismissal(): string | null {
  try {
    return typeof window === "undefined" ? null : window.sessionStorage.getItem(DISMISS_KEY);
  } catch {
    return null;
  }
}

// Slim full-width system strips above the signed-in page content: maintenance and
// read-only states (amber, not dismissible) plus the operator announcement (neutral,
// dismissible per tab-session — keyed on title+body so an EDITED announcement
// re-appears). Best-effort by design: no platform data renders nothing.
//
// The two amber states are ours and are translated; the announcement is not. An operator
// writes that one by hand in the admin console, in whichever language they wrote it, and
// there is no second version of it to pick from — so it renders as authored, in every
// locale. Translating the chrome around it is the most this surface can honestly do.
export function SystemBanner() {
  const platform = usePlatform();
  const t = useT();
  const [dismissedKey, setDismissedKey] = useState<string | null>(storedDismissal);

  if (!platform) return null;

  const announcementKey = `${platform.announcement_title}\u0000${platform.announcement_body}`;
  const showAnnouncement = platform.announcement_active && dismissedKey !== announcementKey;
  if (!platform.maintenance_mode && !platform.read_only && !showAnnouncement) return null;

  const dismiss = () => {
    try {
      window.sessionStorage.setItem(DISMISS_KEY, announcementKey);
    } catch {
      // Storage unavailable (private mode) — dismissal still holds for this render tree.
    }
    setDismissedKey(announcementKey);
  };

  return (
    <div className="space-y-2 px-8 pt-4">
      {platform.maintenance_mode && <AmberStrip>{t("sys.maintenance")}</AmberStrip>}
      {platform.read_only && <AmberStrip>{t("sys.readOnly")}</AmberStrip>}
      {showAnnouncement && (
        <div className="flex items-start gap-3 rounded-lg border border-border bg-card px-4 py-2.5 text-sm">
          <div className="min-w-0 flex-1">
            <span className="font-semibold">{platform.announcement_title}</span>
            {platform.announcement_body && <span className="text-ink-soft"> — {platform.announcement_body}</span>}
          </div>
          <button
            type="button"
            aria-label={t("sys.a11y.dismiss")}
            onClick={dismiss}
            className="shrink-0 rounded-md text-ink-soft outline-none transition-colors hover:text-ink focus-visible:ring-2 focus-visible:ring-ring"
          >
            <X className="size-4" />
          </button>
        </div>
      )}
    </div>
  );
}

function AmberStrip({ children }: { children: string }) {
  return (
    <div className="flex items-center gap-2.5 rounded-lg border border-accent-warn/40 bg-accent-warn/10 px-4 py-2.5 text-sm text-accent-warn">
      <TriangleAlert className="size-4 shrink-0" />
      {children}
    </div>
  );
}
