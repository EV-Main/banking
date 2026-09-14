"use client";

// Sessions & devices: the live refresh-token families at the hub, listed and revocable.
// One list serves both breakpoints — the desktop rail renders it titled, the mobile stack
// pushes it as its own screen under the app bar.

import { useT } from "@evinvest/i18n/react";

import { Loader2 } from "lucide-react";

import { Button, Skeleton } from "@evinvest/uikit";

import type { Session } from "@/shared/contracts";
import { TipAnchor } from "@/shared/tips";
import { Hairline, ListCard, ListCardTitle, Pill } from "@/shared/ui/list-card";
import { deviceOf, metaOf } from "@/views/settings/lib/sessions";

export function SessionsSection({
  titled,
  sessions,
  error,
  busy,
  name,
  onRevoke,
  onRevokeOthers,
}: {
  titled: boolean;
  sessions: Session[] | undefined;
  error: string | null;
  busy: boolean;
  name: string;
  onRevoke: (id: string) => void;
  onRevokeOthers: () => void;
}) {
  const t = useT();
  const loading = sessions === undefined;
  const list = sessions ?? [];
  const hasOthers = list.some((s) => !s.current);
  return (
    <ListCard className="lg:px-6 lg:pb-5.5 lg:pt-2">
      {titled ? (
        <ListCardTitle sub={t("settings.sessionsSub", "Where you're signed in — revoke anything you don't recognise")}>{t("ui.sessionsDevices", "Sessions & devices")}</ListCardTitle>
      ) : (
        <p className="pb-2 pt-3 text-xs font-medium text-ink-soft">{t("settings.sessionsSub", "Where you're signed in — revoke anything you don't recognise")}</p>
      )}

      {error && <p className="mb-2 rounded-md border border-accent-error/40 bg-accent-error/10 px-3 py-2 text-sm text-accent-error">{error}</p>}

      {loading ? (
        [0, 1].map((i) => (
          <div key={i}>
            {i > 0 && <Hairline />}
            <div className="flex items-center gap-3 py-3.5">
              <Skeleton className="size-9 shrink-0 rounded-lg" />
              <Skeleton className="h-4 flex-1" />
              <Skeleton className="h-8 w-20 shrink-0 rounded-md" />
            </div>
          </div>
        ))
      ) : list.length === 0 ? (
        <p className="py-6 text-center text-sm text-ink-soft">{t("settings.noSessions", "No active sessions.")}</p>
      ) : (
        list.map((s, i) => {
          const { label, icon: Icon } = deviceOf(s.user_agent, t);
          return (
            <div key={s.id ?? i}>
              {i > 0 && <Hairline />}
              {/* Device and action stack below `sm` — side by side, both the device label and
                  the ip/last-seen meta were being clipped to an ellipsis on a phone. */}
              <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2.5 py-3.5">
                <div className="flex min-w-0 items-start gap-3">
                  <span className="flex size-9 shrink-0 items-center justify-center rounded-lg border border-border bg-secondary text-ink">
                    <Icon className="size-4.5" />
                  </span>
                  <div className="min-w-0 flex-1">
                    <p className="text-sm font-medium text-ink">{label}</p>
                    <p className="break-words text-xs leading-snug text-ink-soft">{metaOf(s, name, t)}</p>
                  </div>
                </div>
                <div className="flex shrink-0 items-center gap-1.5 pl-12 sm:pl-0">
                  {s.current ? (
                    <>
                      {/* i18n-max: 12 — a `shrink-0` Pill in the row's action slot. */}
                      <Pill>{t("settings.thisDevice", "This device")}</Pill>
                      <TipAnchor anchor="settings.sessions.this-device" />
                    </>
                  ) : (
                    <>
                      {/* i18n-max: 12 — a `shrink-0` Button beside its tip anchor. */}
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={busy}
                        onClick={() => s.id && onRevoke(s.id)}
                        className="border-accent-info/40 text-accent-info hover:text-accent-info"
                      >
                        {t("settings.revoke", "Revoke")}
                      </Button>
                      <TipAnchor anchor="settings.sessions.revoke" />
                    </>
                  )}
                </div>
              </div>
            </div>
          );
        })
      )}

      {!loading && hasOthers && (
        <div className="mb-2 mt-3 flex items-center gap-1.5">
          {/* uikit's Button is `shrink-0 whitespace-nowrap`, so a `w-full` one in a flex
              row keeps its full label width and shoves the tip anchor beside it out of
              the card. `min-w-0 shrink truncate` lets the label give way instead — a
              longer translation is then clipped, not destructive. */}
          {/* i18n-max: 30 */}
          <Button
            variant="outline"
            disabled={busy}
            onClick={onRevokeOthers}
            className="w-full min-w-0 shrink truncate border-accent-info/40 text-accent-info hover:text-accent-info"
          >
            {busy && <Loader2 className="mr-1.5 size-4 animate-spin" />} {t("settings.signOutOthers", "Sign out all other devices")}
          </Button>
          <TipAnchor anchor="settings.sessions.revoke-others" />
        </div>
      )}
    </ListCard>
  );
}
