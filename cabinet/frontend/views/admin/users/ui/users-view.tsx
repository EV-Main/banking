"use client";

import { KeyRound, SearchX, TriangleAlert, X } from "lucide-react";
import { type ReactNode, useId, useState } from "react";

import { useLocale, useT } from "@evinvest/i18n/react";
import { Badge, Button, Card, CardContent, Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle, FieldDescription, Input, Select, SelectContent, SelectItem, SelectTrigger, Skeleton, Spinner, Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@evinvest/uikit";

import { revokeSessions, setKycLevel, type UserFilters } from "@/entities/admin/api/admin-client";
import { adminUserBalanceResource, adminUserResource, usersResource } from "@/entities/admin/model/admin-resource";
import type { AdminUserSummary } from "@/shared/contracts/admin";
import { errorMessage } from "@/shared/lib/api-client";
import { TAG } from "@/shared/lib/cache-tags";
import { cn } from "@/shared/lib/cn";
import { revalidateTag, useResource } from "@/shared/lib/resource";
import { useSession } from "@/shared/lib/use-session";
import { BreakGlassNotice } from "@/shared/ui/break-glass-notice";
import { Settled, StaggerItem } from "@/shared/ui/motion";
import { Panel, PanelPresence, PanelSwap } from "@/shared/ui/motion-panel";
import { ResourceError } from "@/shared/ui/resource-error";
import { TipAnchor, type TipKey } from "@/shared/tips";
import { KYC_LEVELS, type KycLevel, ROLES, accountStanding, ago, formatUsdt, kycLevelLabel, roleLabel, statusLabel, statusTone } from "@/views/admin/lib/format";
import { EDGE_CELL, TABLE_HEAD } from "@/views/admin/lib/table";
import { AdminHeader, AdminScreen, StatusDot } from "@/views/admin/ui/shell";
import { AccountStandingField } from "@/views/admin/users/ui/account-standing-field";
import { RoleField } from "@/views/admin/users/ui/role-field";

export function UsersView() {
  const t = useT();
  const [filters, setFilters] = useState<UserFilters>({});
  const [selected, setSelected] = useState<AdminUserSummary | null>(null);

  // One cache entry per filter set, which also settles the ordering hazard the manual
  // fetch guarded by hand: a search fires per keystroke, and a slower earlier response now
  // lands on its own key instead of clobbering a newer one. Retyping a search already made
  // answers from cache.
  const list = useResource(usersResource, filters);
  const users = list.data ? (list.data.users ?? []) : null;
  const total = list.data?.total ?? "0";
  const error = users || !list.error ? null : errorMessage(list.error, t);

  // This screen no longer cross-references the owner roster, and that is a deletion rather
  // than a regression. The mark it used to draw was inferred: the row's role was compared
  // against `/api/owners` and a mismatch was rendered as "elevated access, no seat". The
  // role reaching this table is now the persisted one, so the two reads can no longer
  // disagree by construction, and a cross-reference that can only fire on a stale or
  // partial roster is a cross-reference that can only lie. Break-glass elevation is
  // reported by the plane as its own flag on the caller — never assembled here out of two
  // lists that happen to differ.
  return (
    <AdminScreen className="space-y-6">
      <AdminHeader eyebrow={t("admin.eyebrow.administer", "Administer")} title={t("nav.users", "Users")} subtitle={t("admin.users.subtitle", "Investors and operators — identities, KYC, roles and sessions")} />

      {/* Above the table, because it changes how every role in it should be read — and
          mounted unconditionally: the component owns the condition (`shared/ui/
          break-glass-notice`) so this screen cannot get it subtly wrong. */}
      <BreakGlassNotice />

      {error && <ResourceError message={error} />}

      <StaggerItem className="flex flex-wrap items-center gap-3">
        <Input
          placeholder={t("admin.users.searchPlaceholder", "Search email or user id…")}
          className="max-w-xs"
          defaultValue={filters.query ?? ""}
          onChange={(e) => setFilters((f) => ({ ...f, query: e.target.value || undefined }))}
        />
        <FilterSelect
          label={t("admin.users.role", "Role")}
          value={filters.role}
          onChange={(role) => setFilters((f) => ({ ...f, role }))}
          options={ROLES}
          optionLabel={(role) => roleLabel(role, t)}
        />
        <FilterSelect
          label={t("admin.col.status", "Status")}
          value={filters.status}
          onChange={(status) => setFilters((f) => ({ ...f, status }))}
          options={["active", "disabled"]}
          optionLabel={(status) => statusLabel(status, t)}
        />
        {/* The count is an ICU plural, not `${n} users`: the noun has to agree with the
            number in most locales, and `#` groups the digits in the reader's own
            convention — which is also what retires the hard-coded `en-US` here. */}
        <span className="ml-auto text-sm text-ink-soft">{t("admin.users.count", "{n, plural, one {# user} other {# users}}", { n: Number(total) })}</span>
      </StaggerItem>

      {/* Table and drawer are one section: the drawer's open/close already owns the
          width of this row, and a second entrance on the same element would be
          animating against it. */}
      <StaggerItem className="flex gap-6">
        <Card className="min-w-0 flex-1">
          <CardContent className="p-0">
            <Settled
              loading={!users}
              skeleton={
                <div className="p-6">
                  <Skeleton className="h-64 w-full" />
                </div>
              }
            >
              {!users ? null : users.length === 0 ? (
                <div className="p-8">
                  <Empty className="border md:p-6">
                    <EmptyHeader>
                      <EmptyMedia variant="icon">
                        <SearchX />
                      </EmptyMedia>
                      <EmptyTitle>{t("admin.users.noMatch", "No users match these filters")}</EmptyTitle>
                      <EmptyDescription>{t("admin.users.noMatchHint", "Loosen the search, role or status filter to see more accounts.")}</EmptyDescription>
                    </EmptyHeader>
                  </Empty>
                </div>
              ) : (
                // `table-fixed` is load-bearing, not tidiness. Under the default
                // auto layout a column is as wide as its content, so `truncate` on
                // an address never engages: when the drawer opens and takes 340px
                // the browser compresses the columns instead, the longer addresses
                // wrap onto a second line, and every row grows taller — the table
                // stretched vertically for the whole width animation. Fixed layout
                // sizes the columns from the header row, so narrowing shortens the
                // addresses and the row heights never move. The other three columns
                // split what User leaves.
                //
                // The floor and the scrolling wrapper are the phone's (banking#327):
                // fixed layout keeps squeezing below any width, and at 400px the User
                // and Role cells were drawn over each other. Under the floor the table
                // scrolls inside the kit's own box instead, the way the payments and
                // payout tables do. The floor stops at `lg`: from there the rail takes
                // 248px and the open drawer another 364px, which leaves the card under
                // 560px at 1024px — and there the squeeze above is the wanted behaviour,
                // not a sideways scroll that hides KYC and Status behind the scrollbar.
                <Table className="table-fixed max-lg:min-w-140">
                  <TableHeader>
                    {/* i18n-max: 8 per header — `table-fixed` sizes the columns from this
                        row, so a header that does not fit wraps instead of widening, and
                        the three right-hand columns share what User leaves. `whitespace-normal`
                        on every head and cell is what lets them wrap at all: the kit's are
                        nowrap, and under a fixed layout nowrap text overprints its neighbour. */}
                    <TableRow>
                      <TableHead className={cn(TABLE_HEAD, EDGE_CELL, "w-1/2 whitespace-normal")}>{t("admin.col.user", "User")}</TableHead>
                      <TableHead className={cn(TABLE_HEAD, EDGE_CELL, "whitespace-normal")}>{t("admin.users.role", "Role")}</TableHead>
                      <TableHead className={cn(TABLE_HEAD, EDGE_CELL, "whitespace-normal")}>{t("admin.users.kyc", "KYC")}</TableHead>
                      <TableHead className={cn(TABLE_HEAD, EDGE_CELL, "whitespace-normal")}>{t("admin.col.status", "Status")}</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {users.map((u) => (
                      // `data-state` is the kit's own selected treatment; the row hover is its too.
                      <TableRow key={u.user_id} onClick={() => setSelected(u)} data-state={selected?.user_id === u.user_id ? "selected" : undefined} className="cursor-pointer">
                        <TableCell className={EDGE_CELL}>
                          {/* The row is clickable for the mouse, but the identity cell carries the
                              real control: a bare `tr onClick` gives the keyboard no way in, and
                              the address is what names the row being opened. */}
                          <button
                            type="button"
                            aria-pressed={selected?.user_id === u.user_id}
                            onClick={() => setSelected(u)}
                            className="flex min-w-0 items-center gap-3 rounded-md text-left outline-none focus-visible:ring-2 focus-visible:ring-ring"
                          >
                            <Avatar email={u.email} />
                            <span className="min-w-0 truncate">{u.email || u.user_id.slice(0, 8)}</span>
                          </button>
                        </TableCell>
                        <TableCell className={cn(EDGE_CELL, "whitespace-normal")}>
                          {/* Stacked, not inline: `table-fixed` sizes this column from an
                              8-character header, so a chip beside the role would push the
                              label out of its own cell in every locale. */}
                          <div className="flex flex-col items-start gap-1">
                            <span>{roleLabel(u.role, t)}</span>
                            {u.role_is_break_glass && <BreakGlassMark />}
                          </div>
                        </TableCell>
                        <TableCell className={cn(EDGE_CELL, "whitespace-normal text-ink-soft")}>{t("admin.users.kycLevelShort", "L{n}", { n: u.kyc_level })}</TableCell>
                        <TableCell className={cn(EDGE_CELL, "whitespace-normal")}>
                          <StatusDot status={u.status} label={statusLabel(u.status, t)} />
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              )}
            </Settled>
          </CardContent>
        </Card>

        {/* Three distinct motions, and they are not interchangeable: opening slides
            the panel in from the right, closing plays that in reverse (which needs
            the node to outlive the state change — hence PanelPresence), and picking
            a different row while the panel is already open leaves the frame where it
            is and cross-fades only the body. The panel is keyed on being open, not
            on which user, or every row click would play a full exit and enter.

            `collapse` is what makes the table widen rather than snap back: the
            drawer hands its 340px and the row's gap back over the course of the
            exit, so the table reflows with it each frame instead of jumping open
            once the drawer has already gone. `overflow-hidden` keeps the card
            inside at its own width while the wrapper narrows — without it the
            drawer's own text reflows on the way out. */}
        <PanelPresence>
          {selected && (
            <Panel
              key="user-drawer"
              // The open width lives here rather than in a class: Panel animates
              // to it, and an inline width from the animation would beat the
              // class anyway. 21.25rem is the `w-85` this used to carry.
              collapse={{ gap: "1.5rem", width: "21.25rem" }}
              className="shrink-0 self-start overflow-hidden"
            >
              <PanelSwap swapKey={selected.user_id}>
                {/* `key` remounts the drawer per user, so an unsaved KYC pick cannot
                    survive into the next reader's row. The KYC control re-seats itself
                    when the LEVEL moves under it (see `KycField`); this handles the other
                    axis — the user under it changing. */}
                <UserDrawer key={selected.user_id} summary={selected} onClose={() => setSelected(null)} />
              </PanelSwap>
            </Panel>
          )}
        </PanelPresence>
      </StaggerItem>
    </AdminScreen>
  );
}

// A `div`, not a `label`: the uikit Select's trigger is a button, which a label has
// nothing to bind to. "All" stays a real, selectable item — clearing the filter has to
// be reachable — and maps back to `undefined`.
function FilterSelect({
  label,
  value,
  onChange,
  options,
  optionLabel,
}: {
  label: string;
  value?: string;
  onChange: (v: string | undefined) => void;
  options: readonly string[];
  /** The wire value is what goes back to the API; this is only what the reader sees. */
  optionLabel: (value: string) => string;
}) {
  const t = useT();
  return (
    <div className="inline-flex items-center gap-2 text-sm">
      <span className="text-ink-soft">{label}:</span>
      <Select value={value ?? ""} onValueChange={(v) => onChange(v || undefined)}>
        <SelectTrigger size="sm" className="border-border bg-secondary">
          <span className="truncate">{value === undefined ? t("ui.all", "All") : optionLabel(value)}</span>
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="">{t("ui.all", "All")}</SelectItem>
          {options.map((o) => (
            <SelectItem key={o} value={o}>
              {optionLabel(o)}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}

function Avatar({ email }: { email: string }) {
  const initials = (email.split("@")[0] ?? "?")
    .split(/[._-]+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((p) => p[0]?.toUpperCase() ?? "")
    .join("");
  return <span className="flex size-8 shrink-0 items-center justify-center rounded-full bg-primary-ink/15 text-xs font-semibold text-primary-ink">{initials || "?"}</span>;
}

function UserDrawer({ summary, onClose }: { summary: AdminUserSummary; onClose: () => void }) {
  const t = useT();
  const locale = useLocale();
  const session = useSession();
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  // Keyed per user, so reopening a row already looked at shows its detail immediately.
  const detail = useResource(adminUserResource, summary.user_id);
  const balanceRead = useResource(adminUserBalanceResource, summary.user_id);
  const profile = detail.data ?? null;
  const balance = balanceRead.data ?? null;
  const error = actionError ?? (profile || !detail.error ? null : errorMessage(detail.error, t));

  const run = async (key: string, fn: () => Promise<unknown>) => {
    setBusy(key);
    setActionError(null);
    try {
      await fn();
      // A role change, a suspension or a KYC bump moves this user's detail AND the row for
      // them in whatever filtered list is behind this drawer. The tag covers both, so the
      // parent no longer needs a refresh counter threaded down here.
      revalidateTag(TAG.adminUsers);
    } catch (e) {
      setActionError(errorMessage(e, t));
    } finally {
      setBusy(null);
    }
  };

  const status = profile?.status ?? summary.status;
  const role = profile?.role ?? summary.role;
  // Falls back to the row that opened this drawer, like `status` and `role` above, so the
  // provenance never contradicts the label it qualifies while the detail read is in flight.
  const breakGlass = profile?.role_is_break_glass ?? summary.role_is_break_glass;

  // The identity plane refuses `SetKycLevel` on the caller's own account, and the BFF now
  // refuses it one hop earlier — so the row the operator is standing on must not offer the
  // control. `userId` is the same handle `AdminUserSummary.user_id` carries: both are the
  // concierge subject, one read from the shell's session, the other from the plane's
  // directory.
  //
  // `session === null` is "not resolved yet" and NOT "someone else" — the reading the admin
  // layout already gives it. Unresolved therefore leaves the control live rather than
  // greying every row for a frame: the refusal is the BFF's to make, and this is only what
  // spares the operator a doomed press.
  const isSelf = session?.user?.userId === summary.user_id;

  return (
    // Fixed width, NOT `w-full`, and this is the whole difference between the
    // drawer being revealed and being squeezed into place. `w-full` made the card
    // follow the wrapper's animating width, so its content relaid out on every
    // frame of the open: the address wrapped, the buttons stacked, and while the
    // panel was narrow the card stood 538px tall against the table's 220 —
    // dragging the whole row's height up and back down again over the animation.
    // Pinned to the panel's open width the layout is computed once and the panel
    // simply clips it, so the entrance is a wipe and nothing reflows.
    <Card className="w-85">
      <CardContent className="space-y-5 py-5">
        <div className="flex items-start justify-between gap-2">
          <div className="min-w-0">
            <p className="truncate font-semibold">{summary.email || summary.user_id.slice(0, 12)}</p>
            <p className="truncate text-xs text-ink-soft">{summary.user_id}</p>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label={t("ui.close", "Close")}
            className="rounded-md text-ink-soft outline-none transition-colors hover:text-ink focus-visible:ring-2 focus-visible:ring-ring"
          >
            <X className="size-4" />
          </button>
        </div>

        {/* i18n-max: 12 per badge — three chips wrap inside a 340px drawer. */}
        <div className="flex flex-wrap gap-1.5 text-xs">
          <Chip>{roleLabel(role, t)}</Chip>
          <Chip>{t("admin.users.kycBadge", "KYC L{n}", { n: profile?.kyc_level ?? summary.kyc_level })}</Chip>
          <span className={cn("rounded-full px-2 py-0.5 font-medium", statusTone(status))}>{statusLabel(status, t)}</span>
          {breakGlass && <BreakGlassMark />}
        </div>

        {/* Spelled out here, where there is room for a sentence — the row only has space
            for the mark. The page-level notice above the table says the same thing about
            the READER; this says it about the account they are looking at, which is a
            different fact and can be true when the other is not. */}
        {breakGlass && <p className="text-xs leading-relaxed text-ink-soft">{t("admin.users.breakGlassExplainer", "This role came from the OWNER_SUBJECTS allowlist, not from the register. It is real access and it seats nobody — the consilium does not count it, and it lapses as soon as the fund has its first owner.")}</p>}

        {error && (
          <p className="flex items-center gap-2 text-xs text-accent-error">
            <TriangleAlert className="size-3.5" /> {error}
          </p>
        )}

        <Section title={t("admin.users.identity", "Identity")}>
          <Row label={t("admin.users.joined", "Joined")} value={ago(summary.created_at, t)} />
          <Row
            label={t("admin.users.tokenVersion", "Token version")}
            value={t("admin.users.tokenVersionValue", "v{n}", { n: profile?.token_version ?? summary.token_version })}
            tip="admin.users.identity.token-version"
          />
          <Row label={t("admin.users.balance", "Balance")} value={balance ? `${formatUsdt(balance.amount, locale)} USDT` : "—"} />
          {/* The two parts of `amount`: an operator asked why a withdrawal is refused needs
              to see that money parked in a compartment cannot leave until moved to main. */}
          <Row label={t("admin.users.balanceMain", "· main")} value={balance ? `${formatUsdt(balance.main, locale)} USDT` : "—"} />
          <Row label={t("admin.users.balanceInCompartments", "· in compartments")} value={balance ? `${formatUsdt(balance.in_compartments, locale)} USDT` : "—"} />
        </Section>

        <Section title={t("admin.users.accessSecurity", "Access & security")}>
          <RoleField userId={summary.user_id} role={role} busy={busy} run={run} />
          <KycField
            level={profile?.kyc_level ?? summary.kyc_level}
            busy={busy === "kyc"}
            isSelf={isSelf}
            onSave={(next) => run("kyc", () => setKycLevel(summary.user_id, next))}
          />
          <Button type="button" variant="outline" size="sm" className="mt-2 w-full border-accent-error/40 text-accent-error hover:bg-accent-error/10" disabled={busy === "revoke"} onClick={() => run("revoke", () => revokeSessions(summary.user_id))}>
            {busy === "revoke" ? <Spinner aria-hidden /> : null}
            {t("admin.users.revokeAllSessions", "Revoke all sessions")}
          </Button>
          <p className="flex items-center gap-1.5 pt-1 text-xs text-ink-soft">
            {t("admin.users.revokeNote", "Bumps token_version — invalidates every JWT issued to this user.")}
            <TipAnchor anchor="admin.users.access.revoke-sessions" />
          </p>
        </Section>

        {/* Three cases, not two. Which control an account gets is decided by WHY it is
            blocked (`suspended_by`), never by `status` alone — see `AccountStandingField`.
            The tip anchor moves inside it, beside the action it is about. */}
        <AccountStandingField
          userId={summary.user_id}
          standing={accountStanding({
            status,
            suspended_by: profile?.suspended_by ?? summary.suspended_by,
            hold_expires_at: profile?.hold_expires_at ?? summary.hold_expires_at,
          })}
          busy={busy}
          run={run}
        />

      </CardContent>
    </Card>
  );
}

/**
 * The KYC tier control: four named tiers and a Save that only lights when something changed.
 *
 * It reads as an odd twin of {@link RoleField} directly above, which commits the moment you
 * pick — so the asymmetry is the point. A role has ONE author: the operator sitting in this
 * drawer. A KYC tier has two, because the identity plane raises it on its own when a
 * verification completes, and it can do that while this drawer is open. Committing on pick
 * puts those two writers in a race that the reader cannot see and the slower one wins.
 *
 * Which is also why the draft is re-seated from the incoming level rather than defended
 * against it: a tier that moved underneath the reader displaces an unsaved pick instead of
 * being hidden by one. Losing a pick nobody committed is the cheap failure; showing (and
 * then re-sending) a tier the plane no longer holds is the expensive one. The re-seat runs
 * during render on purpose — an effect would paint the stale tier for a frame first, and a
 * ref written in render is the pattern two of this app's standing lint errors are already
 * about.
 *
 * `isSelf` is the one row it refuses to be a control on at all. An operator holding
 * `KycManage` can raise anybody's tier except their own, and the identity plane enforces
 * that — so offering the selector here would be offering a press that answers 403. This is
 * the same shape as the owner seat {@link RoleField} greys out, and the same reason: a
 * console that offers a move the plane refuses is worse than one that explains the refusal.
 * The sentence below the control is what carries the explanation; the BFF sends the same
 * one back, keyed, for the caller that reaches it anyway.
 */
function KycField({ level, busy, isSelf, onSave }: { level: number; busy: boolean; isSelf: boolean; onSave: (level: KycLevel) => void }) {
  const t = useT();
  const hintId = useId();
  const [draft, setDraft] = useState(level);
  const [seated, setSeated] = useState(level);
  if (seated !== level) {
    setSeated(level);
    setDraft(level);
  }

  // `draft` carries the wire's `number` because the tier this user ALREADY holds may be one
  // this console does not know — a rung the identity plane grows later. `picked` narrows it
  // back to the closed list, so an unknown tier can be read and replaced but never re-sent,
  // and Save stays dark until the reader has chosen one of the four.
  const picked = KYC_LEVELS.find((l) => l === draft);
  const dirty = picked !== undefined && picked !== level;

  // Two reasons a press is refused here, and they must not be announced the same way. `busy`
  // is transient, so the native `disabled` is the honest state. `isSelf` is policy — and a
  // control that leaves the tab order takes its explanation with it, leaving a keyboard or
  // screen-reader operator with a gap where the reason should be. So the operator's own row
  // keeps both controls focusable under `aria-disabled` (which also keeps the kit's
  // `focus-visible:ring-ring`) and points them at the sentence below; the refusal itself is
  // enforced by the controlled `open` and the `onClick` guard rather than by the browser.
  return (
    <div className="flex flex-col gap-1.5 py-1">
      {/* A `div` and not a `label`, for the reason `FilterSelect` gives: the uikit trigger
          is a button, which a label has nothing to bind to. */}
      <div className="flex items-center justify-between gap-2 text-sm">
        <span className="flex items-center gap-1.5 text-ink-soft">
          {t("ui.kycLevel", "KYC level")}
          <TipAnchor anchor="admin.users.access.kyc-level" />
        </span>
        <Select
          value={String(draft)}
          onValueChange={(v) => setDraft(Number(v))}
          // Held shut at the root, not by a no-op handler on the trigger: the kit calls the
          // trigger's `onClick` and then toggles `open` regardless, so a controlled `open` is
          // the only thing that keeps the list closed while the trigger stays focusable.
          open={isSelf ? false : undefined}
        >
          {/* The transient refusal is still `disabled`, and still on the trigger, which is the
              button — `RoleField` spells out why the uikit `Select` root is not the place for
              it. The tier the account already holds renders in every state, so the operator can
              READ their own standing here. */}
          <SelectTrigger
            size="sm"
            className="border-border bg-secondary aria-disabled:cursor-not-allowed aria-disabled:opacity-50"
            disabled={busy}
            aria-disabled={isSelf ? true : undefined}
            aria-describedby={isSelf ? hintId : undefined}
          >
            <span className="truncate">{kycLevelLabel(draft, t)}</span>
          </SelectTrigger>
          <SelectContent>
            {/* i18n-max: 16 per tier — a `size="sm"` trigger inside a 340px drawer. */}
            {KYC_LEVELS.map((l) => (
              <SelectItem key={l} value={String(l)}>
                {kycLevelLabel(l, t)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      {/* `picked` is re-checked rather than asserted: `dirty` already implies it, but the
          two are separate facts to the type checker and an assertion would be the place
          this drifts. */}
      <Button
        type="button"
        variant="outline"
        size="sm"
        className="self-end aria-disabled:cursor-not-allowed aria-disabled:opacity-50"
        // Never the native attribute on the own row: that is the one refusal whose reason has
        // to be reachable, and `disabled` would drop the button out of the tab order before
        // `aria-describedby` could be read out.
        disabled={!isSelf && (busy || !dirty)}
        aria-disabled={isSelf ? true : undefined}
        aria-describedby={isSelf ? hintId : undefined}
        onClick={() => {
          if (isSelf || picked === undefined) return;
          onSave(picked);
        }}
      >
        {busy ? <Spinner aria-hidden /> : null}
        {t("ui.save", "Save")}
      </Button>
      {/* Below the control rather than instead of it, the way the owner seat's sentence sits
          under the role select: a greyed-out select with no sentence reads as a fault. The `id`
          is what makes it the two controls' accessible description rather than a sentence only
          a sighted reader knows to pair with them. */}
      {isSelf && (
        <FieldDescription id={hintId} className="text-xs leading-relaxed">
          {t("admin.users.kycSelf", "Your own verification tier is not set from here. Another holder of KycManage has to change it.")}
        </FieldDescription>
      )}
    </div>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="space-y-1 border-t border-border pt-4">
      <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{title}</p>
      {children}
    </div>
  );
}

function Row({ label, value, tip }: { label: string; value: string; tip?: TipKey }) {
  return (
    <div className="flex items-center justify-between py-1 text-sm">
      <span className="flex items-center gap-1.5 text-ink-soft">
        {label}
        {tip && <TipAnchor anchor={tip} />}
      </span>
      <span className="tabular-nums">{value}</span>
    </div>
  );
}

function Chip({ children }: { children: ReactNode }) {
  return <span className="rounded-full bg-ink/5 px-2 py-0.5 font-medium text-ink">{children}</span>;
}

/**
 * "This role came from the allowlist, not the register."
 *
 * Drawn strictly from `role_is_break_glass` on the row the API sent. Its deleted
 * predecessor, `NoSeatMark`, computed the same idea by subtracting the owner roster from
 * the user list — which could only be as right as the older of two reads, and marked
 * nothing at all whenever the roster was forbidden, which was most of the time. One field
 * from one response cannot disagree with itself.
 *
 * Warning-toned, never destructive, for the same reason the page-level notice is: this is
 * a deliberate arrangement on a fund that has no owners yet. Colouring it as a fault would
 * push someone to "fix" it by granting the role — the one move the mechanism exists to
 * stop, and the one `SetRole` now refuses outright.
 */
function BreakGlassMark() {
  const t = useT();
  return (
    <Badge variant="outline" className="gap-1 whitespace-nowrap border-accent-warn/40 text-accent-warn">
      <KeyRound className="size-3" aria-hidden />
      {/* i18n-max: 14 — this sits in a table column sized from an 8-character header. */}
      {t("admin.users.breakGlassRole", "Emergency access")}
    </Badge>
  );
}
