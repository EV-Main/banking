"use client";

import { Activity, Loader2, RefreshCw, TriangleAlert } from "lucide-react";
import { useState } from "react";

import { useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, Skeleton } from "@evinvest/uikit";

import { unparkEvent } from "@/entities/admin/api/admin-client";
import { overviewResource, parkedEventsResource } from "@/entities/admin/model/admin-resource";
import { errorMessage, RequestError } from "@/shared/lib/api-client";
import { useResource } from "@/shared/lib/resource";
import { TipAnchor, type TipKey } from "@/shared/tips";
import { ago, statusLabel } from "@/views/admin/lib/format";
import { StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { AdminHeader, AdminScreen, StatusDot } from "@/views/admin/ui/shell";

export function OverviewView() {
  const t = useT();
  const [refreshing, setRefreshing] = useState(false);
  const [unparkError, setUnparkError] = useState<string | null>(null);
  const [unparking, setUnparking] = useState<string | null>(null);
  // Rows whose unpark POST succeeded but whose re-read failed — still listed, but they must
  // not offer a second unpark. A successful re-read drops them from the list.
  const [unparked, setUnparked] = useState<ReadonlySet<string>>(new Set());
  const [refetchError, setRefetchError] = useState<string | null>(null);

  // Both reads are cached, so returning to Overview shows the fleet grid and the backlog it
  // last held and settles them behind. They carry the same tag and are always refreshed
  // together — the "Parked rows" KPI and the table below it must never disagree.
  const overviewRead = useResource(overviewResource);
  const parkedRead = useResource(parkedEventsResource);
  const overview = overviewRead.data ?? null;
  const parked = parkedRead.data ? (parkedRead.data.events ?? []) : null;
  const error = overview || !overviewRead.error ? null : errorMessage(overviewRead.error, t);
  // Best-effort: a money plane that isn't connected renders as a muted hint, not an error
  // banner — the fleet grid above must stay useful without it.
  const parkedHint = parked || !parkedRead.error ? null : errorMessage(parkedRead.error, t);

  // Manual "Run health check". Both, so the KPI and the table agree.
  const load = () => {
    setRefreshing(true);
    setRefetchError(null);
    void Promise.allSettled([overviewRead.refresh(), parkedRead.refresh()]).then(() => {
      setUnparked(new Set());
      setRefreshing(false);
    });
  };

  const unpark = async (seq: string) => {
    setUnparking(seq);
    setUnparkError(null);
    setRefetchError(null);
    try {
      const { ok } = await unparkEvent(seq);
      // A `RequestError` rather than a bare `Error`: the transport call succeeded, so the
      // refusal has to carry its own catalogue key to reach the reader in their language.
      if (!ok) throw new RequestError("the hub declined the unpark", 200, "err.unparkDeclined");
    } catch (e) {
      setUnparkError(errorMessage(e, t));
      setUnparking(null);
      return;
    }
    // The POST succeeded — mark the row unparked before the re-read so a failed re-read
    // can't leave an enabled Unpark button on an already-unparked event.
    setUnparked((prev) => new Set(prev).add(seq));
    try {
      // Both, so the "Parked rows" KPI drops together with the list.
      await Promise.all([parkedRead.refresh(), overviewRead.refresh()]);
      setUnparked(new Set());
    } catch (e) {
      setRefetchError(errorMessage(e, t));
    } finally {
      setUnparking(null);
    }
  };

  const healthy = overview?.services.filter((s) => s.status === "healthy").length ?? 0;
  const totalServices = overview?.services.length ?? 0;

  return (
    <AdminScreen className="space-y-8">
      <AdminHeader
        eyebrow={t("admin.eyebrow.administer")}
        title={t("nav.overview")}
        subtitle={t("admin.overview.subtitle")}
        action={
          <Button type="button" variant="outline" size="sm" disabled={refreshing} onClick={load}>
            <RefreshCw className={refreshing ? "size-4 animate-spin" : "size-4"} /> {t("admin.overview.runHealthCheck")}
          </Button>
        }
      />

      {error && <ResourceError message={error} />}

      <StaggerItem className="grid gap-4 sm:grid-cols-2 lg:grid-cols-5">
        <Kpi label={t("admin.overview.kpi.servicesHealthy")} value={overview ? `${healthy}/${totalServices}` : undefined} tone="text-positive" />
        <Kpi
          label={t("admin.overview.kpi.parkedRows")}
          value={overview?.parked_rows}
          hint={t("admin.overview.kpi.parkedRowsHint")}
          tone={overview && overview.parked_rows !== "0" ? "text-accent-error" : undefined}
          tip="admin.overview.kpi.parked-rows"
        />
        <Kpi label={t("admin.overview.kpi.dispatchBacklog")} value={overview?.backlog} hint={t("admin.overview.kpi.dispatchBacklogHint")} tip="admin.overview.kpi.dispatch-backlog" />
        <Kpi
          label={t("admin.overview.kpi.oldestBacklog")}
          value={overview ? t("admin.overview.kpi.oldestBacklogValue", { n: overview.oldest_backlog_age_secs }) : undefined}
          hint={t("admin.overview.kpi.oldestBacklogHint")}
          tip="admin.overview.kpi.oldest-backlog"
        />
        <Kpi
          label={t("admin.overview.kpi.deadKeySignings")}
          value={overview?.unseal_failures}
          hint={t("admin.overview.kpi.deadKeySigningsHint")}
          tone={overview && overview.unseal_failures !== "0" ? "text-accent-error" : undefined}
          tip="admin.overview.kpi.dead-key-signings"
        />
      </StaggerItem>

      <StaggerItem className="grid gap-6 lg:grid-cols-3">
        <Card className="lg:col-span-2">
          <CardContent className="space-y-4 py-5">
            <div>
              <h2 className="text-base font-semibold">{t("admin.overview.fleetHealth")}</h2>
              <p className="text-xs text-ink-soft">{t("admin.overview.fleetHealthSub")}</p>
            </div>
            {!overview ? (
              <Skeleton className="h-48 w-full" />
            ) : (
              <table className="w-full text-sm">
                <thead>
                  {/* i18n-max: 14 per header — an auto-layout table; a long header widens
                      its column at the expense of the two beside it. */}
                  <tr className="border-b border-border text-left text-xs uppercase tracking-wide text-ink-soft">
                    <th className="py-2 font-medium">{t("admin.overview.col.service")}</th>
                    <th className="py-2 font-medium">{t("admin.overview.col.kind")}</th>
                    <th className="py-2 font-medium">{t("admin.col.status")}</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-border">
                  {overview.services.map((s) => (
                    <tr key={s.name}>
                      <td className="py-2.5 font-medium">{s.name}</td>
                      <td className="py-2.5 capitalize text-ink-soft">{s.kind}</td>
                      <td className="py-2.5">
                        <StatusDot status={s.status} label={statusLabel(s.status, t)} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardContent className="space-y-3 py-5">
            <div className="flex items-center gap-2">
              <Activity className="size-4 text-accent-debug" />
              <h2 className="text-base font-semibold">{t("admin.overview.errorsAnalytics")}</h2>
            </div>
            {/* `Sentry` and `PostHog` are product names and stay English by policy. */}
            <ObsPanel label="Sentry" hint={t("admin.overview.obs.sentry")} />
            <ObsPanel label="PostHog" hint={t("admin.overview.obs.posthog")} />
            <ObsPanel label={t("admin.overview.obs.eventStreamLabel")} hint={t("admin.overview.obs.eventStream")} />
          </CardContent>
        </Card>
      </StaggerItem>

      <StaggerItem as={Card}>
        <CardContent className="space-y-4 py-5">
          <div>
            <h2 className="text-base font-semibold">{t("admin.overview.parkedEvents")}</h2>
            <p className="text-xs text-ink-soft">{t("admin.overview.parkedEventsSub")}</p>
          </div>
          {unparkError && (
            <p className="flex items-center gap-2 text-xs text-accent-error">
              <TriangleAlert className="size-3.5" /> {unparkError}
            </p>
          )}
          {refetchError && (
            <p className="flex items-center gap-2 text-xs text-accent-warn">
              <TriangleAlert className="size-3.5" /> {t("admin.overview.unparkRefetchFailed", { error: refetchError })}
            </p>
          )}
          {!parked ? (
            <Skeleton className="h-16 w-full" />
          ) : parkedHint ? (
            <p className="text-sm text-ink-soft">{parkedHint}</p>
          ) : parked.length === 0 ? (
            <p className="text-sm text-ink-soft">{t("admin.overview.noParkedEvents")}</p>
          ) : (
            <table className="w-full text-sm">
              <thead>
                {/* i18n-max: 14 per header — auto-layout table; the Reason cell is the one
                    that gives width back, and it is already `truncate`d. */}
                <tr className="border-b border-border text-left text-xs uppercase tracking-wide text-ink-soft">
                  <th className="py-2 font-medium">{t("admin.overview.col.seq")}</th>
                  <th className="py-2 font-medium">{t("admin.overview.col.event")}</th>
                  <th className="py-2 font-medium">
                    <span className="flex items-center gap-1.5">
                      {t("admin.overview.col.reason")}
                      <TipAnchor anchor="admin.overview.parked.reason" />
                    </span>
                  </th>
                  <th className="py-2 font-medium">{t("admin.overview.col.parked")}</th>
                  <th className="py-2 font-medium" />
                </tr>
              </thead>
              <tbody className="divide-y divide-border">
                {parked.map((e) => (
                  <tr key={e.seq}>
                    <td className="py-2.5 font-mono-tech text-xs text-ink-soft">{e.seq}</td>
                    <td className="py-2.5">
                      <p className="font-medium">{e.kind}</p>
                      <p className="font-mono-tech text-xs text-ink-soft">
                        {e.aggregate} · {e.aggregate_id}
                      </p>
                    </td>
                    <td className="py-2.5 text-ink-soft">
                      <div className="max-w-70 truncate" title={e.reason}>
                        {e.reason || "—"}
                      </div>
                    </td>
                    <td className="whitespace-nowrap py-2.5 text-ink-soft">{ago(e.parked_at, t)}</td>
                    <td className="py-2.5 text-right">
                      <div className="flex items-center justify-end gap-2">
                        {/* i18n-max: 12 per badge — three chips and a button share this cell. */}
                        {e.compensated && (
                          <span className="flex items-center gap-1.5 whitespace-nowrap rounded-full bg-ink/5 px-2 py-0.5 text-xs font-medium text-ink">
                            {t("admin.overview.compensated")}
                            <TipAnchor anchor="admin.overview.parked.compensated" />
                          </span>
                        )}
                        {unparked.has(e.seq) && (
                          <span className="whitespace-nowrap rounded-full bg-positive/15 px-2 py-0.5 text-xs font-medium text-positive">{t("admin.overview.unparked")}</span>
                        )}
                        <Button
                          type="button"
                          variant="outline"
                          size="sm"
                          disabled={e.compensated || unparked.has(e.seq) || unparking !== null}
                          onClick={() => void unpark(e.seq)}
                        >
                          {unparking === e.seq ? <Loader2 className="size-3.5 animate-spin" /> : null}
                          {t("admin.overview.unpark")}
                        </Button>
                        <TipAnchor anchor="admin.overview.parked.unpark" />
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </CardContent>
      </StaggerItem>
    </AdminScreen>
  );
}

function Kpi({ label, value, hint, tone, tip }: { label: string; value: string | undefined; hint?: string; tone?: string; tip?: TipKey }) {
  return (
    <Card>
      <CardContent className="space-y-1 py-5">
        <div className="flex items-center gap-1.5">
          <p className="text-xs uppercase tracking-wide text-ink-soft">{label}</p>
          {tip && <TipAnchor anchor={tip} />}
        </div>
        {value === undefined ? <Skeleton className="mt-1 h-8 w-20" /> : <p className={`text-3xl font-semibold tabular-nums ${tone ?? ""}`}>{value}</p>}
        {hint && <p className="text-xs text-ink-soft">{hint}</p>}
      </CardContent>
    </Card>
  );
}

function ObsPanel({ label, hint }: { label: string; hint: string }) {
  return (
    <div className="rounded-lg border border-dashed border-border p-3">
      <p className="text-xs font-semibold text-ink">{label}</p>
      <p className="text-xs text-ink-soft">{hint}</p>
    </div>
  );
}
