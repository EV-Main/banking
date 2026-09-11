"use client";

import { TriangleAlert } from "lucide-react";
import { type ReactNode, useState } from "react";

import { useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, Input, Skeleton } from "@evinvest/uikit";

import { setAnnouncement, setFeatureFlag, setMaintenance, setReadOnly } from "@/entities/admin/api/admin-client";
import { cabinetConfigResource, mfeRegistryResource } from "@/entities/admin/model/admin-resource";
import type { CabinetConfig, FeatureFlag } from "@/shared/contracts/admin";
import { errorMessage } from "@/shared/lib/api-client";
import { useResource } from "@/shared/lib/resource";
import { TipAnchor, type TipKey } from "@/shared/tips";
import { StaggerItem } from "@/shared/ui/motion";
import { AdminHeader, AdminScreen, StatusDot, Toggle } from "@/views/admin/ui/shell";

export function CabinetView() {
  const t = useT();
  const [writeError, setWriteError] = useState<string | null>(null);

  const read = useResource(cabinetConfigResource);
  const config = read.data ?? null;
  const mfes = useResource(mfeRegistryResource).data ?? null;
  const error = writeError ?? (config || !read.error ? null : errorMessage(read.error, t));

  const platform = config?.platform;

  // Each write answers with the piece it changed, so the new config is published straight
  // into the cache — the system banner reading the same platform config follows without a
  // refetch, and leaving this screen and coming back shows the switch where it was left.
  const publish = (next: Partial<typeof config> & object) => {
    if (config) cabinetConfigResource.publish({ ...config, ...next });
  };

  const toggleFlag = async (flag: FeatureFlag) => {
    try {
      publish({ platform: await setFeatureFlag({ key: flag.key, description: flag.description, enabled: !flag.enabled, rollout: flag.rollout }) });
    } catch (e) {
      setWriteError(errorMessage(e, t));
    }
  };

  const toggleMaintenance = async (enabled: boolean) => {
    try {
      publish({ platform: await setMaintenance(enabled) });
    } catch (e) {
      setWriteError(errorMessage(e, t));
    }
  };

  const toggleReadOnly = async (enabled: boolean) => {
    try {
      publish({ read_only: (await setReadOnly(enabled)).read_only });
    } catch (e) {
      setWriteError(errorMessage(e, t));
    }
  };

  return (
    <AdminScreen className="space-y-8">
      <AdminHeader eyebrow={t("admin.eyebrow.administer")} title={t("nav.cabinet")} subtitle={t("admin.cabinet.subtitle")} />

      {error && (
        <StaggerItem as="p" className="flex items-center gap-2 text-sm text-accent-error">
          <TriangleAlert className="size-4" /> {error}
        </StaggerItem>
      )}

      <div className="grid gap-6 lg:grid-cols-2">
        <Panel title={t("admin.cabinet.mfeRegistry")} subtitle={t("admin.cabinet.mfeRegistrySub")}>
          {!mfes ? (
            <Skeleton className="h-32 w-full" />
          ) : mfes.length === 0 ? (
            <p className="py-6 text-center text-sm text-ink-soft">{t("admin.cabinet.noMfes")}</p>
          ) : (
            <div className="divide-y divide-border">
              {mfes.map((m) => (
                <div key={m.tag} className="flex items-center justify-between gap-3 py-3">
                  <div className="min-w-0">
                    <p className="truncate text-sm font-medium">{m.name}</p>
                    <p className="truncate font-mono-tech text-xs text-ink-soft">{m.tag}</p>
                  </div>
                  <div className="flex shrink-0 items-center gap-3">
                    <span className="rounded-md bg-ink/5 px-2 py-0.5 text-xs capitalize text-ink-soft">{m.kind}</span>
                    <StatusDot status="healthy" label={t("admin.cabinet.registered")} />
                  </div>
                </div>
              ))}
            </div>
          )}
        </Panel>

        <Panel title={t("admin.cabinet.featureFlags")} subtitle={t("admin.cabinet.featureFlagsSub")}>
          {!platform ? (
            <Skeleton className="h-32 w-full" />
          ) : platform.flags.length === 0 ? (
            <p className="py-6 text-center text-sm text-ink-soft">{t("admin.cabinet.noFlags")}</p>
          ) : (
            <div className="divide-y divide-border">
              {platform.flags.map((f) => (
                <div key={f.key} className="flex items-center justify-between gap-3 py-3">
                  <div className="min-w-0">
                    <p className="truncate font-mono-tech text-sm">{f.key}</p>
                    <p className="flex items-center gap-1.5 truncate text-xs text-ink-soft">
                      {t("admin.cabinet.flagRollout", { pct: f.rollout })} {f.description ? `· ${f.description}` : ""}
                      <TipAnchor anchor="admin.cabinet.flags.rollout" />
                    </p>
                  </div>
                  <Toggle on={f.enabled} onChange={() => toggleFlag(f)} label={f.key} />
                </div>
              ))}
            </div>
          )}
        </Panel>

        <Panel title={t("admin.cabinet.announcement")} subtitle={t("admin.cabinet.announcementSub")}>
          {!config ? <Skeleton className="h-28 w-full" /> : <AnnouncementForm config={config} onSaved={(next) => publish({ platform: next })} onError={setWriteError} />}
        </Panel>

        <Panel title={t("admin.cabinet.maintenanceOps")} subtitle={t("admin.cabinet.maintenanceOpsSub")}>
          {!config ? (
            <Skeleton className="h-28 w-full" />
          ) : (
            <div className="space-y-4">
              <ToggleRow
                label={t("admin.cabinet.maintenanceMode")}
                hint={t("admin.cabinet.maintenanceModeHint")}
                on={config.platform.maintenance_mode}
                onChange={toggleMaintenance}
                tip="admin.cabinet.maintenance"
              />
              <ToggleRow
                label={t("admin.cabinet.readOnlyMode")}
                hint={t("admin.cabinet.readOnlyModeHint")}
                on={config.read_only}
                onChange={toggleReadOnly}
                tip="admin.cabinet.readonly"
              />
            </div>
          )}
        </Panel>
      </div>
    </AdminScreen>
  );
}

// The grid this sits in is a plain `div`, which motion's variants pass straight
// through — so each panel takes its own place in the screen's sequence rather than
// the four of them arriving as one slab.
function Panel({ title, subtitle, children }: { title: string; subtitle: string; children: ReactNode }) {
  return (
    <StaggerItem as={Card}>
      <CardContent className="space-y-4 py-5">
        <div>
          <h2 className="text-base font-semibold">{title}</h2>
          <p className="text-xs text-ink-soft">{subtitle}</p>
        </div>
        {children}
      </CardContent>
    </StaggerItem>
  );
}

function ToggleRow({ label, hint, on, onChange, tip }: { label: string; hint: string; on: boolean; onChange: (next: boolean) => void; tip?: TipKey }) {
  return (
    <div className="flex items-center justify-between gap-3">
      <div>
        <div className="flex items-center gap-1.5">
          <p className="text-sm">{label}</p>
          {tip && <TipAnchor anchor={tip} />}
        </div>
        <p className="text-xs text-ink-soft">{hint}</p>
      </div>
      <Toggle on={on} onChange={onChange} label={label} />
    </div>
  );
}

function AnnouncementForm({ config, onSaved, onError }: { config: CabinetConfig; onSaved: (next: CabinetConfig["platform"]) => void; onError: (e: string) => void }) {
  const t = useT();
  const [title, setTitle] = useState(config.platform.announcement_title);
  const [body, setBody] = useState(config.platform.announcement_body);
  const [active, setActive] = useState(config.platform.announcement_active);
  const [saving, setSaving] = useState(false);

  const save = async () => {
    setSaving(true);
    try {
      const next = await setAnnouncement({ title, body, active });
      onSaved(next);
    } catch (e) {
      onError(errorMessage(e, t));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="space-y-3">
      <Input value={title} onChange={(e) => setTitle(e.target.value)} placeholder={t("admin.cabinet.placeholder.announcementTitle")} />
      <Input value={body} onChange={(e) => setBody(e.target.value)} placeholder={t("admin.cabinet.placeholder.announcementBody")} />
      <div className="flex items-center justify-between">
        <ToggleRow label={t("admin.cabinet.live")} hint={t("admin.cabinet.liveHint")} on={active} onChange={setActive} tip="admin.cabinet.announcement.live" />
      </div>
      <Button type="button" variant="outline" size="sm" disabled={saving} onClick={save}>
        {t("admin.cabinet.saveAnnouncement")}
      </Button>
    </div>
  );
}
