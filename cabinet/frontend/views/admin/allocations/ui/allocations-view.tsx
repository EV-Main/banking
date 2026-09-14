"use client";

import type { Translate } from "@evinvest/i18n";
import { Loader2, Plus } from "lucide-react";
import { useState } from "react";

import { useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, Input, Select, SelectContent, SelectItem, SelectTrigger, Skeleton } from "@evinvest/uikit";

import { registerAllocation, setAllocationState, updateAllocation, type AllocationWrite } from "@/entities/admin/api/admin-client";
import { adminAllocationsResource } from "@/entities/admin/model/admin-resource";
import type { Allocation, AllocationIcon, AllocationState } from "@/shared/contracts/admin";
import { errorMessage } from "@/shared/lib/api-client";
import { TAG } from "@/shared/lib/cache-tags";
import { cn } from "@/shared/lib/cn";
import { ALLOCATION_ICONS, ProductIcon } from "@/shared/ui/icons/products";
import { revalidateTag, useResource } from "@/shared/lib/resource";
import { Settled, StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { compactUnits, stateLabel } from "@/views/admin/lib/format";
import { AdminHeader, AdminScreen } from "@/views/admin/ui/shell";

const TEAL_CTA = "bg-accent-debug text-background hover:bg-accent-debug/90";

// The registry's own vocabulary, rendered. `closed` is amber rather than destructive:
// it stops new subscriptions but investors can still redeem out of it, so it is a
// wind-down, not a failure.
const STATE_TONE: Record<AllocationState, string> = {
  draft: "border-border text-ink-soft",
  open: "border-positive/40 bg-positive/10 text-positive",
  closed: "border-accent-warn/40 bg-accent-warn/10 text-accent-warn",
};

// Catalogue keys, not finished prose: this map is module scope, so it holds what to say
// rather than the words, and the row resolves it against the reader's locale.
// Named per icon rather than `t(`admin.alloc.icon.${value}`)`: an interpolated key is
// invisible to the extractor, so these ten never reached a translator.
const ICON_LABELS: Record<AllocationIcon, (t: Translate) => string> = {
  "fund": t => t("admin.alloc.icon.fund", "Fund"),
  "real_estate": t => t("admin.alloc.icon.real_estate", "Real estate"),
  "trading": t => t("admin.alloc.icon.trading", "Trading"),
  "yield": t => t("admin.alloc.icon.yield", "Yield"),
  "venture": t => t("admin.alloc.icon.venture", "Venture"),
  "treasury": t => t("admin.alloc.icon.treasury", "Treasury"),
  "commodity": t => t("admin.alloc.icon.commodity", "Commodity"),
  "credit": t => t("admin.alloc.icon.credit", "Credit"),
  "index": t => t("admin.alloc.icon.index", "Index"),
  "arbitrage": t => t("admin.alloc.icon.arbitrage", "Arbitrage"),
};

const STATE_HINT: Record<AllocationState, (t: Translate) => string> = {
  draft: t => t("admin.alloc.hint.draft", "Registered — accepts no money until opened"),
  open: t => t("admin.alloc.hint.open", "Accepting subscriptions and redemptions"),
  closed: t => t("admin.alloc.hint.closed", "Closed to new money; redemptions still settle"),
};

/**
 * The icon picker, shared by the register form and the row editor so the two cannot drift
 * apart — they write the same field through the same full-replace endpoint.
 *
 * No `SelectValue`: it renders the raw stored value, and here that is the wire's
 * `real_estate` rather than the operator's "Real estate". A `<span>` inside the trigger is
 * the house fix for that — see the rail picker in `views/admin/treasury/ui/treasury-view.tsx`.
 *
 * Each option carries its glyph beside its name, because the glyph is the thing actually
 * being chosen; a list of words alone would be picking a picture blind.
 */
function IconSelect({ value, onChange, className }: { value: AllocationIcon; onChange: (icon: AllocationIcon) => void; className?: string }) {
  const t = useT();
  return (
    <Select
      value={value}
      // Narrowed by lookup rather than asserted: the uikit hands back a bare `string`, and
      // a cast here would let a typo in an option's `value` through to the BFF as a 400.
      onValueChange={(v) => {
        const picked = ALLOCATION_ICONS.find((i) => i === v);
        if (picked) onChange(picked);
      }}
    >
      <SelectTrigger className={cn("w-full border-border bg-secondary", className)}>
        <span className="flex min-w-0 items-center gap-1.5">
          <ProductIcon icon={value} className="size-3.5 shrink-0" />
          <span className="truncate">{ICON_LABELS[value](t)}</span>
        </span>
      </SelectTrigger>
      <SelectContent>
        {ALLOCATION_ICONS.map((icon) => (
          <SelectItem key={icon} value={icon}>
            <ProductIcon icon={icon} className="size-3.5 shrink-0" />
            {ICON_LABELS[icon](t)}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

export function AllocationsView() {
  const t = useT();
  const [actionError, setActionError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);

  const read = useResource(adminAllocationsResource);
  const rows = read.data ? (read.data.allocations ?? []) : null;
  const error = actionError ?? (read.data || !read.error ? null : errorMessage(read.error, t));

  const run = async (key: string, fn: () => Promise<unknown>) => {
    setBusy(key);
    setActionError(null);
    try {
      await fn();
      // Registering, renaming or opening a fund also changes what investors see: the rail's
      // Products group, the fund picker, and the names on the activity rows all read the
      // investor-facing catalog. Naming that tag is what keeps an operator change from
      // sitting invisible behind a five-minute window on every open investor tab.
      revalidateTag(TAG.catalog);
      await read.refresh();
      return true;
    } catch (e) {
      setActionError(errorMessage(e, t));
      return false;
    } finally {
      setBusy(null);
    }
  };

  return (
    <AdminScreen className="space-y-8">
      <AdminHeader
        eyebrow={t("admin.eyebrow.administer", "Administer")}
        title={t("nav.allocations", "Allocations")}
        subtitle={t("admin.alloc.subtitle", "The registry of investable products — a fund exists only once it is registered here")}
        action={
          <Button type="button" className={cn(TEAL_CTA)} onClick={() => setAdding((v) => !v)}>
            <Plus className="size-4" />
            {t("admin.alloc.register", "Register allocation")}
          </Button>
        }
      />

      {error && <ResourceError message={error} />}

      {adding && <RegisterForm busy={busy === "register"} onCancel={() => setAdding(false)} onSubmit={async (body) => (await run("register", () => registerAllocation(body))) && setAdding(false)} />}

      <StaggerItem as="section" className="space-y-3">
        <p className="flex items-center gap-2 text-xs font-semibold uppercase tracking-widest text-ink-soft">
          {t("admin.alloc.registry", "Registry")}
          {rows && <span className="rounded-full bg-accent-debug/15 px-2 py-0.5 text-xs font-semibold text-accent-debug">{rows.length}</span>}
        </p>
        <Card>
          <CardContent className="p-0">
            <Settled
              loading={!rows}
              skeleton={
                <div className="p-6">
                  <Skeleton className="h-32 w-full" />
                </div>
              }
            >
              {!rows ? null : rows.length === 0 ? (
                <p className="p-8 text-center text-sm text-ink-soft">{t("admin.alloc.empty", "No allocations registered yet — until one is, every subscription is refused.")}</p>
              ) : (
                <table className="w-full text-sm">
                  <thead>
                    {/* i18n-max: 14 per header — auto-layout table with no scroll wrapper;
                        a long header widens its column and squeezes the Product cell. */}
                    <tr className="border-b border-border text-left text-xs uppercase tracking-wide text-ink-soft">
                      <th className="px-5 py-3 font-medium">{t("admin.alloc.col.product", "Product")}</th>
                      <th className="px-5 py-3 font-medium">{t("admin.alloc.col.serviceId", "Service id")}</th>
                      <th className="px-5 py-3 font-medium">{t("admin.col.state", "State")}</th>
                      <th className="px-5 py-3 font-medium">{t("admin.alloc.col.unitCap", "Unit cap")}</th>
                      <th className="px-5 py-3 text-right font-medium">{t("admin.col.actions", "Actions")}</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y divide-border">
                    {rows.map((row) => (
                      <AllocationRow
                        // Keyed on the edit flag as well as the service, so opening the
                        // editor remounts the row and reseeds its fields from the row as
                        // it stands now. Keyed on the service alone, the draft state
                        // outlived a cancel: edit, change, cancel, reopen, then save a
                        // title, and the discarded change went with it. Harmless-looking
                        // for a title the operator retypes, but an icon silently reverting
                        // to what it was two edits ago is the kind of thing nobody catches.
                        // The key is stable while the editor is open, so a background
                        // revalidation cannot wipe what is being typed.
                        key={`${row.service}:${editing === row.service}`}
                        row={row}
                        busy={busy === row.service}
                        editing={editing === row.service}
                        onEdit={() => setEditing((current) => (current === row.service ? null : row.service))}
                        onSave={async (body) => (await run(row.service, () => updateAllocation(body))) && setEditing(null)}
                        onToggle={() => run(row.service, () => setAllocationState(row.service, row.state === "open" ? "closed" : "open"))}
                      />
                    ))}
                  </tbody>
                </table>
              )}
            </Settled>
          </CardContent>
        </Card>
        {/* One key for the whole paragraph, with the state name interpolated: a translator
            has to be able to move `draft` to wherever the sentence puts it in their
            language, which splitting the note around the `<span>` would forbid. The cost
            is the word's mono styling, which is decoration rather than meaning here. */}
        <p className="max-w-3xl text-xs text-ink-soft">{t("admin.alloc.footnote", "Registration lands in {state}, which takes no money — opening is a separate decision. Closing stops new subscriptions only: investors can always redeem out of a closed product, so winding one down never strands their units. The service id is permanent once registered. The unit cap bounds how many units may ever be issued — resize it on the Valuation screen, where the issued figure it is judged against is on the same page.", { state: t("admin.state.draft", "Draft") })}</p>
      </StaggerItem>
    </AdminScreen>
  );
}

function AllocationRow({
  row,
  busy,
  editing,
  onEdit,
  onSave,
  onToggle,
}: {
  row: Allocation;
  busy: boolean;
  editing: boolean;
  onEdit: () => void;
  onSave: (body: AllocationWrite) => void;
  onToggle: () => void;
}) {
  const t = useT();
  const [title, setTitle] = useState(row.title);
  const [summary, setSummary] = useState(row.summary);
  // Seeded from the row, so an operator who opens the editor to fix a typo in the title
  // still submits the icon the product already wears. `/update` is a full replace: a body
  // without `icon` is read as "none chosen" and resets the product to `fund`, so the
  // editor holding the current value is what stops a rename from repainting the rail.
  // `?? "fund"` covers the row arriving from a cache written before the field existed.
  const [icon, setIcon] = useState<AllocationIcon>(row.icon ?? "fund");

  return (
    <>
      <tr>
        <td className="px-5 py-3">
          {/* The mark is shown in the row, not only inside the editor: it is what an
              investor sees in the rail, so an operator must be able to check it without
              opening a form that could then be saved by accident. */}
          <div className="flex items-center gap-2.5">
            <ProductIcon icon={row.icon} className="size-4 shrink-0 text-ink-soft" />
            <div className="min-w-0">
              <p className="font-medium">{row.title}</p>
              {row.summary && <p className="text-xs text-ink-soft">{row.summary}</p>}
            </div>
          </div>
        </td>
        <td className="px-5 py-3 font-mono-tech text-xs text-ink-soft">{row.service}</td>
        <td className="px-5 py-3">
          {/* i18n-max: 12 — a chip in a table cell. The value is authored in display
              case rather than lowercased and fixed with CSS `capitalize`: that rule
              title-cases every word, which is invisible on a one-word English enum and
              wrong the moment it is translated ("en cours" → "En Cours"). */}
          <span className={cn("inline-flex items-center whitespace-nowrap rounded-full border px-2 py-0.5 text-xs font-medium", STATE_TONE[row.state])} title={STATE_HINT[row.state](t)}>
            {stateLabel(row.state, t)}
          </span>
        </td>
        {/* Read-only here: resizing the supply is a money decision and lives on the
            Valuation screen, next to the issued figure it has to be judged against. */}
        <td className="px-5 py-3 tabular-nums text-ink-soft">{compactUnits(row.unit_cap)}</td>
        <td className="px-5 py-3">
          {/* i18n-max: 12 per verb — two `shrink-0` Buttons share this cell, and every
              character widens the Actions column at the Product column's expense. */}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="outline" size="sm" disabled={busy} onClick={onEdit}>
              {editing ? t("ui.cancel", "Cancel") : t("ui.edit", "Edit")}
            </Button>
            <Button type="button" variant="outline" size="sm" disabled={busy} onClick={onToggle}>
              {busy ? <Loader2 className="size-4 animate-spin" /> : row.state === "open" ? t("admin.alloc.close", "Close") : t("admin.alloc.open", "Open")}
            </Button>
          </div>
        </td>
      </tr>
      {editing && (
        <tr className="bg-ink/[0.03]">
          <td colSpan={5} className="px-5 py-4">
            <div className="flex flex-wrap items-end gap-3">
              {/* `flex flex-col`, not `block` + `space-y`: the uikit Input is `inline-flex`,
                  so a narrow one shares the line with its label unless the column is
                  explicit. */}
              <label className="flex w-56 flex-col gap-1.5">
                <span className="text-xs text-ink-soft">{t("admin.alloc.field.title", "Title")}</span>
                <Input value={title} onChange={(e) => setTitle(e.target.value)} className="w-full" />
              </label>
              <label className="flex min-w-56 flex-1 flex-col gap-1.5">
                <span className="text-xs text-ink-soft">{t("admin.alloc.field.summary", "Summary")}</span>
                <Input value={summary} onChange={(e) => setSummary(e.target.value)} placeholder={t("admin.alloc.placeholder.summary", "One line for the catalog card")} className="w-full" />
              </label>
              {/* Not a `<label>`: the trigger is a button, and wrapping it would make the
                  caption a second click target that reopens the popup it just closed. */}
              <div className="flex w-44 flex-col gap-1.5">
                <span className="text-xs text-ink-soft">{t("admin.alloc.field.icon", "Icon")}</span>
                <IconSelect value={icon} onChange={setIcon} />
              </div>
              <Button type="button" className={cn(TEAL_CTA)} disabled={busy || !title.trim()} onClick={() => onSave({ service: row.service, title, summary, icon })}>
                {t("ui.save", "Save")}
              </Button>
            </div>
          </td>
        </tr>
      )}
    </>
  );
}

function RegisterForm({ busy, onCancel, onSubmit }: { busy: boolean; onCancel: () => void; onSubmit: (body: AllocationWrite) => void }) {
  const t = useT();
  const [service, setService] = useState("");
  const [title, setTitle] = useState("");
  const [summary, setSummary] = useState("");
  // Pre-selected rather than blank, and `fund` specifically because that is what the hub
  // stores when none is sent — so the form shows the outcome of leaving it alone.
  const [icon, setIcon] = useState<AllocationIcon>("fund");

  // Mirrors `ServiceId::parse` — the hub rejects anything else, so say so before the
  // round-trip rather than surfacing a validation error after it.
  const slugOk = /^[A-Za-z0-9_-]{1,64}$/.test(service);

  return (
    // A `StaggerItem` although it is not part of the page's own arrival: it mounts when
    // the operator asks for it, and the parent's variants carry it in the same way.
    <StaggerItem as={Card}>
      <CardContent className="space-y-4 py-6">
        <div className="grid gap-4 md:grid-cols-3">
          <label className="flex flex-col gap-1.5">
            <span className="text-sm text-ink-soft">{t("admin.alloc.col.serviceId", "Service id")}</span>
            {/* The two placeholders are format examples, not prose — a slug and a proper
                noun — so they stay as they are in every locale. */}
            <Input value={service} onChange={(e) => setService(e.target.value.trim())} placeholder="quy-nhon-fund" spellCheck={false} className="w-full font-mono-tech" />
            <span className={cn("text-xs", service && !slugOk ? "text-accent-error" : "text-ink-soft")}>
              {service && !slugOk ? t("admin.alloc.slugInvalid", "Letters, digits, - and _ only (1–64)") : t("admin.alloc.slugHint", "Permanent — every fund RPC keys on it")}
            </span>
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-sm text-ink-soft">{t("admin.alloc.field.title", "Title")}</span>
            <Input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Quy Nhon Fund" className="w-full" />
          </label>
          {/* Not a `<label>` — see the row editor: the trigger is a button, so a wrapping
              label would toggle the popup a second time. */}
          <div className="flex flex-col gap-1.5">
            <span className="text-sm text-ink-soft">{t("admin.alloc.field.icon", "Icon")}</span>
            <IconSelect value={icon} onChange={setIcon} />
          </div>
          <label className="flex flex-col gap-1.5">
            <span className="text-sm text-ink-soft">{t("admin.alloc.field.summary", "Summary")}</span>
            <Input value={summary} onChange={(e) => setSummary(e.target.value)} placeholder={t("admin.alloc.placeholder.summary", "One line for the catalog card")} className="w-full" />
          </label>
        </div>
        <div className="flex items-center gap-3">
          <p className="min-w-0 text-xs text-ink-soft">{t("admin.alloc.registerHint", "Registers in draft — open it when the product is ready to take money.")}</p>
          {/* i18n-max: 12 per verb — both Buttons are `shrink-0` beside the hint above. */}
          <Button type="button" variant="outline" className="ml-auto" onClick={onCancel}>
            {t("ui.cancel", "Cancel")}
          </Button>
          <Button type="button" className={cn(TEAL_CTA)} disabled={busy || !slugOk || !title.trim()} onClick={() => onSubmit({ service, title, summary, icon })}>
            {busy ? <Loader2 className="size-4 animate-spin" /> : null}
            {t("admin.alloc.registerSubmit", "Register")}
          </Button>
        </div>
      </CardContent>
    </StaggerItem>
  );
}
