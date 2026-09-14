"use client";

// The owners' room: who the owners are, what is currently being decided, and the two things
// an owner can start from here (proposing a removal, and standing down).
//
// Everything on this page is read from the server and nothing is computed from a socket
// frame. The stream says only that a revision moved; the cache re-reads, and these
// components render the answer. That is why there is no optimistic update anywhere in this
// file — a tally is a fact about a decision that moves money or ends an ownership, and the
// only place it is ever computed is inside one Postgres transaction with the request row
// locked (docs/CONSILIUM.md, policy 14, 21).

import { Banknote, Loader2, ShieldAlert, Users } from "lucide-react";
import { Fragment, useState, type CSSProperties } from "react";

import { useLocale, useT } from "@evinvest/i18n/react";
import {
  Badge,
  Button,
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
  Input,
  Item,
  ItemContent,
  ItemDescription,
  ItemGroup,
  ItemMedia,
  ItemSeparator,
  ItemTitle,
  Label,
  Progress,
  Separator,
  Skeleton,
} from "@evinvest/uikit";

import { useConsiliumStream, type StreamStatus } from "@/entities/governance/model/consilium-socket";
import {
  admissionsResource,
  cancelConsilium,
  consiliumResource,
  ownersResource,
  refreshGovernance,
  removalsResource,
  resignOwnership,
} from "@/entities/governance/model/governance-resource";
import { profileResource } from "@/entities/user/model/profile-resource";
import type { Consilium, ConsiliumList, OwnerList } from "@/shared/contracts/governance";
import { RequestError, errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { expiresIn, formatDay, formatMoment, hasStamp } from "@/shared/lib/datetime";
import { hashPrefix } from "@/shared/lib/hash";
import { initialsOf } from "@/shared/lib/identity";
import { formatExactUsdt } from "@/shared/lib/money";
import { networkLabel } from "@/shared/lib/rail";
import { useResource } from "@/shared/lib/resource";
import { BreakGlassNotice } from "@/shared/ui/break-glass-notice";
import { Link } from "@/shared/ui/cabinet-link";
import { SECTION_STAGGER, Settled, Stagger, StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { EMPTY_BOX, isSettled, stateLabel, stateTone } from "@/views/consilium/lib/format";
import {
  everyReadFailed,
  knownValue,
  mapRead,
  ownerCount,
  readOf,
  rosterState,
  type Read,
} from "@/views/consilium/lib/reads";
import { AdmissionList, ProposeAdmission } from "@/views/consilium/ui/admissions";
import { PayoutSkeleton, RosterSkeleton } from "@/views/consilium/ui/loading";
import { ReadFailure } from "@/views/consilium/ui/read-failure";
import { ProposeRemoval, RemovalList } from "@/views/consilium/ui/removals";

const isForbidden = (error: unknown): boolean => error instanceof RequestError && error.status === 403;

export function ConsiliumView() {
  const t = useT();

  const owners = useResource(ownersResource);
  const removals = useResource(removalsResource);
  const admissions = useResource(admissionsResource);
  const consilia = useResource(consiliumResource);
  const profile = useResource(profileResource);

  // Decided before the stream is acquired, not after: a non-owner who opens this URL would
  // otherwise hold a websocket and a 20-second poll against four endpoints that answer 403
  // forever, for a page they are about to be told they cannot see.
  const forbidden = [owners.error, removals.error, admissions.error, consilia.error].some(isForbidden);
  const stream = useConsiliumStream(!forbidden);

  if (forbidden) {
    return (
      <div className="flex flex-col gap-6 px-4 py-10 lg:px-8">
        {/* Here, and not only on the page below, because this is the branch a break-glass
            operator actually lands on: their role is `operator`/`admin`, every read here is
            owner-only, so all four answer 403 and the room shuts before they see a word of
            it. Without the notice the page would say "you cannot see this" and leave the
            reason — the register is empty and they hold no seat — entirely unstated, which
            is the same silence this whole change is about. */}
        <BreakGlassNotice className="mx-auto w-full max-w-160" />
        <Empty className="mx-auto max-w-160 rounded-xl border border-border bg-card p-6 md:p-8">
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <ShieldAlert />
            </EmptyMedia>
            <EmptyTitle>{t("consilium.forbidden.title", "This room is for the fund's owners")}</EmptyTitle>
            <EmptyDescription>{t("consilium.forbidden.body", "Only owners can see and take part in what is decided here. If that should include you, ask an existing owner.")}</EmptyDescription>
          </EmptyHeader>
          <EmptyContent>
            <Button asChild variant="outline">
              <Link href="/">{t("status.backHome", "Back to home")}</Link>
            </Button>
          </EmptyContent>
        </Empty>
      </div>
    );
  }

  const userId = profile.data?.user_id ?? null;
  const myEmail = profile.data?.email ?? "";

  // One `Read` per endpoint, and nothing below this line touches `.data` with a fallback.
  // `owners.data?.items ?? []` is what turned a 404 into an empty roster, which the header
  // then published as "0 owners" and the propose form as "you are the only owner listed" —
  // see `views/consilium/lib/reads.ts` for why the shape, not the care of the author, is
  // what has to prevent that.
  const rosterRead = readOf(owners);
  const payoutsRead = readOf(consilia);
  // `items ?? []` on a read that ARRIVED: proto3 JSON omits an empty repeated field, so an
  // absent list here means there are none, not that nothing came back.
  const removalsRead = mapRead(readOf(removals), (list) => (list.items ?? []).filter((r) => !isSettled(r.state, r.decided_at)));
  const admissionsRead = mapRead(readOf(admissions), (list) => (list.items ?? []).filter((a) => !isSettled(a.state, a.decided_at)));

  const roster = knownValue(rosterRead);

  // All four reads are the same session against the same BFF, so they fail together far
  // more often than they fail alone. When every one of them is down, four identical boxes
  // stacked down the page report one event four times and imply four independent faults;
  // one failure with one retry is the truth. A PARTIAL failure is never folded in here —
  // "the roster loaded but the payouts did not" says which half of the room can be trusted,
  // and that is worth a card of its own.
  const outage = everyReadFailed([rosterRead, payoutsRead, removalsRead, admissionsRead]);
  const refreshingAll = owners.isValidating || removals.isValidating || admissions.isValidating || consilia.isValidating;

  return (
    <Stagger
      step={SECTION_STAGGER}
      className="grid grid-cols-1 gap-4 px-4 pb-6 pt-5 lg:gap-6 lg:px-8 lg:pb-7 lg:pt-6 xl:grid-cols-(--consilium-columns) xl:items-start"
      style={{ "--consilium-columns": "minmax(0, 1fr) 360px" } as CSSProperties}
    >
      <StaggerItem className="flex flex-wrap items-start justify-between gap-3 xl:col-span-2">
        <div className="flex min-w-0 flex-col gap-1">
          <h1 className="text-2xl font-semibold leading-tight text-ink">{t("consilium.title", "Consilium")}</h1>
          <p className="text-sm text-ink-soft">{t("consilium.sub", "The owners' room — who the owners are, and what is being decided.")}</p>
        </div>
        <StreamChip status={stream.status} />
      </StaggerItem>

      <BreakGlassNotice className="xl:col-span-2" />

      {roster?.below_payout_floor && (
        // Not styled as an error: nothing has failed. It is a standing fact about the fund
        // that changes what it can do, and it is the first thing an owner should know.
        // Body copy is `text-ink`, not muted — muted on a tinted ground is the
        // contrast failure AGENTS.md calls out by name.
        <StaggerItem className="flex items-start gap-3 rounded-xl border border-accent-warn/40 bg-accent-warn/10 px-4 py-3.5 xl:col-span-2">
          <ShieldAlert className="mt-0.5 size-4 shrink-0 text-accent-warn" />
          <div className="flex min-w-0 flex-col gap-1">
            <p className="text-sm font-semibold text-ink">{t("consilium.floor.title", "The fund cannot authorise a payout")}</p>
            <p className="text-sm leading-relaxed text-ink">{t("consilium.floor.body", "A payout needs more than half of the owners to agree, and with {n, plural, one {# owner} other {# owners}} that bar can no longer be cleared. Adding owners restores it.", { n: roster.items?.length ?? 0 })}</p>
          </div>
        </StaggerItem>
      )}

      {outage ? (
        // A plain grid child rather than a StaggerItem: this mounts after the page's
        // entrance has already finished, so it must not depend on that sequence being
        // re-run to become visible. See `./read-failure.tsx`.
        <div className="xl:col-span-2">
          <ReadFailure
            className="mx-auto max-w-160 rounded-xl p-6 md:p-8"
            title={t("consilium.unavailable.title", "The owners' room could not be loaded")}
            body={t("consilium.unavailable.body", "Every one of the reads this room is built from failed, so the page cannot show who owns the fund, what is being decided, or where any vote stands. Nothing here should be trusted until it loads. Try again — and if it keeps failing, this data is unreachable rather than empty, which is worth raising.")}
            onRetry={refreshGovernance}
            retrying={refreshingAll}
          />
        </div>
      ) : (
        <>
          <StaggerItem className="flex flex-col gap-4 lg:gap-6 xl:col-start-1 xl:row-start-3">
            <PayoutSection read={payoutsRead} onRetry={() => void consilia.refresh()} retrying={consilia.isValidating} />
            {/* Admission before removal, and both before the forms that open them. A fund
                whose roster is empty — which is a reachable state, not a fault — can do
                exactly one thing from this page, and it is admit someone; putting the way
                out below the way to shrink further would be an odd thing to read. */}
            <AdmissionList
              read={admissionsRead}
              userId={userId}
              onRetry={() => void admissions.refresh()}
              retrying={admissions.isValidating}
            />
            <RemovalList read={removalsRead} userId={userId} onRetry={() => void removals.refresh()} retrying={removals.isValidating} />
            <ProposeAdmission roster={rosterRead} onRetry={() => void owners.refresh()} retrying={owners.isValidating} />
            {/* Always rendered, and it is the roster's own `Read` that decides what it
                shows: gated on `!isLoading` it vanished from the layout while the owners
                were in flight, and gated on nothing at all it announced "There is nobody to
                propose" on the strength of a read that had failed. */}
            <ProposeRemoval roster={rosterRead} userId={userId} onRetry={() => void owners.refresh()} retrying={owners.isValidating} />
          </StaggerItem>

          <StaggerItem className="flex flex-col gap-4 lg:gap-6 xl:col-start-2 xl:row-start-3">
            <Roster read={rosterRead} userId={userId} onRetry={() => void owners.refresh()} retrying={owners.isValidating} />
            <ResignCard email={myEmail} loadingProfile={profile.isLoading} />
          </StaggerItem>
        </>
      )}
    </Stagger>
  );
}

/**
 * How the page is being kept current — quiet, and honest about the difference.
 *
 * It says "reconnecting" rather than hiding the fact, because the alternative is a page
 * that looks live and is not. The room stays correct either way: the fallback poll is
 * running behind this label (see `entities/governance/model/consilium-socket.ts`).
 */
function StreamChip({ status }: { status: StreamStatus }) {
  const t = useT();
  if (status === "idle") return null;
  const live = status === "live";
  return (
    <Badge variant="outline" className="gap-1.5 rounded-full font-medium text-ink-soft">
      <span
        className={cn(
          "size-1.5 rounded-full",
          live ? "bg-positive" : status === "paused" ? "bg-ink-soft" : "animate-pulse bg-accent-warn",
        )}
      />
      {(live ? t("consilium.stream.live", "Live") : status === "paused" ? t("consilium.stream.paused", "Paused") : t("consilium.stream.reconnecting", "Reconnecting"))}
    </Badge>
  );
}

function Roster({
  read,
  userId,
  onRetry,
  retrying,
}: {
  read: Read<OwnerList>;
  userId: string | null;
  onRetry: () => void;
  retrying: boolean;
}) {
  const t = useT();
  const locale = useLocale();
  const count = ownerCount(read);
  const owners = knownValue(read)?.items ?? [];
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t("consilium.roster.title", "Owners")}</CardTitle>
        {/* How many owners there are is a fact about the fund, so it is printed only when
            the roster arrived. Printed unconditionally, it reported a 404 as "0 owners" —
            in the same header as a card that was showing the reader nothing at all. */}
        {count !== null && (
          <CardAction className="text-xs font-medium tabular-nums text-ink-soft">
            {t("consilium.roster.count", "{n, plural, one {# owner} other {# owners}}", { n: count })}
          </CardAction>
        )}
      </CardHeader>
      <CardContent>
        <Settled loading={read.status === "loading"} skeleton={<RosterSkeleton />}>
          {read.status === "loading" ? null : read.status === "failed" ? (
            <ReadFailure
              title={t("consilium.roster.failedTitle", "The owner list did not load")}
              body={t("consilium.roster.failedBody", "Until it loads, the page cannot say who owns this fund or how many owners it has, so it says nothing rather than guessing at it. The rest of the page is unaffected.")}
              onRetry={onRetry}
              retrying={retrying}
            />
          ) : rosterState(read) === "unseated" ? (
            // An empty roster used to be treated as impossible: the copy said it "should
            // not be possible" and told the reader to report it. It is in fact the one
            // state a brand-new deployment starts in — genesis has not run — and the copy
            // now says which of its two causes this is and what closes it.
            //
            // Genesis seats the founders once, at start-up, from `OWNER_SUBJECTS`, and only
            // while the persisted roster is empty. It refuses to run unless at least TWO
            // listed ids resolve to existing accounts, because admission needs a peer
            // besides the initiator: a fund seeded with one owner could never admit the
            // second and no API path can empty the roster to let the seed retry, so a lone
            // seat is an unrecoverable dead end and seating nobody is the better failure
            // (docs/CONSILIUM.md, § Genesis). So an empty roster means the list was unset,
            // or too few of its ids had accounts yet — a configuration to finish, not a bug
            // to file, and the operator reading this is the person who can finish it.
            //
            // The reader is also inside the emergency-access window while this shows, which
            // is why the copy names it: it is the only period in the fund's life when an
            // environment variable confers anything, and the first successful seeding ends
            // it permanently.
            //
            // Dashed frame, muted, no destructive tint: this is an operational state. The
            // failed read above keeps the solid red box, because the two are different
            // sentences and the reader should tell them apart before reading a word.
            <Empty className={EMPTY_BOX}>
              <EmptyHeader>
                <EmptyMedia variant="icon">
                  <Users />
                </EmptyMedia>
                <EmptyTitle>{t("consilium.roster.emptyTitle", "No owner holds a seat yet")}</EmptyTitle>
                <EmptyDescription>{t("consilium.roster.emptyBody", "Genesis has not run yet. At start-up the service seats everyone on the OWNER_SUBJECTS list who already has an account — but only if at least two of them resolve. A sole owner could never admit the second, so seating one is refused outright rather than left as a dead end.")}</EmptyDescription>
              </EmptyHeader>
              <EmptyContent>
                <p className="text-sm leading-relaxed text-ink-soft">{t("consilium.roster.emptyNext", "Add the missing people to OWNER_SUBJECTS, have each of them sign in once so the account exists, and restart the service. Until the first seat is written, the operators on that list are working under emergency access — temporary by construction: the first successful seeding closes it permanently, and from then on a seat is only ever granted by a consilium of the existing owners.")}</p>
              </EmptyContent>
            </Empty>
          ) : (
            <ItemGroup>
              {owners.map((owner, i) => (
                <Fragment key={owner.user_id}>
                  {i > 0 && <ItemSeparator />}
                  <Item size="sm" className="px-0">
                    <ItemMedia>
                      <span className="flex size-8 shrink-0 items-center justify-center rounded-full bg-accent-debug/15 text-xs font-semibold text-accent-debug">
                        {initialsOf(owner.email)}
                      </span>
                    </ItemMedia>
                    <ItemContent className="min-w-0 gap-0.5">
                      <ItemTitle className="block w-auto truncate font-medium">
                        {owner.display_name || owner.email}
                        {owner.user_id === userId && (
                          <span className="ml-1.5 text-xs font-normal text-ink-soft">{t("consilium.roster.you", "(you)")}</span>
                        )}
                      </ItemTitle>
                      <ItemDescription className="truncate text-xs tabular-nums">
                        {t("consilium.roster.since", "Owner since {at}", { at: formatDay(owner.owner_since, locale) })}
                      </ItemDescription>
                    </ItemContent>
                  </Item>
                </Fragment>
              ))}
            </ItemGroup>
          )}
        </Settled>
      </CardContent>
    </Card>
  );
}

function PayoutSection({
  read,
  onRetry,
  retrying,
}: {
  read: Read<ConsiliumList>;
  onRetry: () => void;
  retrying: boolean;
}) {
  const t = useT();
  const locale = useLocale();
  const items = knownValue(read)?.items ?? [];
  const open = items.filter((c) => !isSettled(c.state, c.decided_at));
  const past = items.filter((c) => isSettled(c.state, c.decided_at));
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t("consilium.payout.title", "Fund payout")}</CardTitle>
        <CardDescription>{t("consilium.payout.sub", "Paying out money the fund has earned. One request at a time, and never one person's decision.")}</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-5">
        <Settled loading={read.status === "loading"} skeleton={<PayoutSkeleton />}>
          {read.status === "loading" ? null : read.status === "failed" ? (
            // In place of the empty state, never beside it. "No payout is open" is a claim
            // about the fund, and a read that failed is not entitled to make it.
            <ReadFailure
              title={t("consilium.payout.failedTitle", "Fund payouts did not load")}
              body={t("consilium.payout.failedBody", "Until it loads, the page cannot tell you whether a payout is open and waiting on the other owners — so do not read this as there being none. The rest of the page is unaffected.")}
              onRetry={onRetry}
              retrying={retrying}
            />
          ) : open.length === 0 ? (
            <Empty className={EMPTY_BOX}>
              <EmptyHeader>
                <EmptyMedia variant="icon">
                  <Banknote />
                </EmptyMedia>
                <EmptyTitle>{t("consilium.payout.emptyTitle", "No payout is open")}</EmptyTitle>
                <EmptyDescription>{t("consilium.payout.emptyBody", "When someone asks to pay out the fund's earnings, it appears here with its running tally, and every owner is emailed a link to answer from.")}</EmptyDescription>
              </EmptyHeader>
              <EmptyContent>
                <Button asChild variant="outline">
                  <Link href="/admin/revenue">{t("consilium.payout.emptyAction", "Go to fund revenue")}</Link>
                </Button>
              </EmptyContent>
            </Empty>
          ) : (
            <div className="flex flex-col gap-5">
              {open.map((consilium) => (
                <OpenPayout key={consilium.id} consilium={consilium} />
              ))}
            </div>
          )}
        </Settled>

        {past.length > 0 && (
          <>
            <Separator />
            <div className="flex flex-col gap-2">
              {/* Nothing is deleted: a rejected, expired or failed request stays readable,
                  because the ledger and the governance record have to reconcile after the
                  fact (docs/CONSILIUM.md § Audit). */}
              <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("consilium.payout.past", "Earlier requests")}</p>
              <ItemGroup>
                {past.map((consilium, i) => (
                  <Fragment key={consilium.id}>
                    {i > 0 && <ItemSeparator />}
                    <Item size="sm" className="px-0">
                      <ItemContent className="min-w-0 gap-0.5">
                        <ItemTitle className="block w-auto truncate font-medium tabular-nums">
                          {formatExactUsdt(consilium.revenue_payout?.amount)} USDT · {networkLabel(consilium.revenue_payout?.network)}
                        </ItemTitle>
                        <ItemDescription className="truncate text-xs tabular-nums">
                          {/* `??` cannot do this: an undecided consilium carries the STRING "0", which is truthy. */}
                          {formatMoment(hasStamp(consilium.decided_at) ? consilium.decided_at : consilium.created_at, locale)}
                        </ItemDescription>
                      </ItemContent>
                      <Badge variant="outline" className={cn("shrink-0", stateTone(consilium.state))}>
                        {stateLabel(consilium.state, t)}
                      </Badge>
                    </Item>
                  </Fragment>
                ))}
              </ItemGroup>
            </div>
          </>
        )}
      </CardContent>
    </Card>
  );
}

function OpenPayout({ consilium }: { consilium: Consilium }) {
  const t = useT();
  const locale = useLocale();
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [error, setError] = useState<unknown>(null);

  const payout = consilium.revenue_payout;
  const threshold = consilium.threshold ?? 0;
  const approvals = consilium.approvals ?? 0;
  const progress = threshold > 0 ? Math.min(100, Math.round((approvals / threshold) * 100)) : 0;

  const cancel = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await cancelConsilium(consilium.id);
      setConfirming(false);
    } catch (cause) {
      setError(cause);
    } finally {
      setBusy(false);
    }
  };

  // No inner card: policy 16 allows at most one open payout at a time, so a second frame
  // inside the section's own Card would be chrome around a single child.
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <p className="text-2xl font-semibold leading-none tabular-nums text-ink">
          {formatExactUsdt(payout?.amount)}
          <span className="ml-2 text-sm font-medium text-ink-soft">USDT</span>
        </p>
        <Badge variant="outline" className={stateTone(consilium.state)}>
          {stateLabel(consilium.state, t)}
        </Badge>
      </div>

      {/* Full, monospace, wrapped rather than truncated — the same rule as the approval
          email and the approval page. An owner who checks the address here and approves it
          there must be looking at the same characters (policy 13). */}
      <p className="break-all rounded-lg border border-border bg-secondary px-3 py-2.5 font-mono-tech text-xs leading-relaxed text-ink">
        {payout?.address || "—"}
      </p>

      <div className="flex flex-col gap-1.5 text-xs text-ink-soft">
        <span className="tabular-nums">{t("consilium.payout.network", "Network · {network}", { network: networkLabel(payout?.network) })}</span>
        <span className="font-mono-tech">{t("consilium.payout.fingerprint", "Fingerprint · {hash}", { hash: hashPrefix(consilium.payload_hash) })}</span>
        <span>{t("consilium.payout.openedBy", "Opened by {initiator}", { initiator: consilium.initiator_email })}</span>
        <span className="tabular-nums">
          {t("consilium.payout.expires", "Expires {at} · {left}", { at: formatMoment(consilium.expires_at, locale), left: expiresIn(consilium.expires_at, t) })}
        </span>
      </div>

      <div className="flex flex-col gap-2">
        <div className="flex items-baseline justify-between gap-3">
          <span className="text-sm font-medium tabular-nums text-ink">
            {t("consilium.payout.tally", "{approvals} of {threshold, plural, one {# approval} other {# approvals}}", { approvals, threshold })}
          </span>
          <span className="text-xs tabular-nums text-ink-soft">{t("consilium.payout.owners", "{n, plural, one {# owner} other {# owners}}", { n: consilium.owner_count ?? 0 })}</span>
        </div>
        <Progress value={progress} className="h-1.5" aria-hidden />
        <p className="text-xs text-ink-soft">{t("consilium.payout.voteByEmail", "Owners answer from the link emailed to them, not from this page — the code in that message is what makes an answer deliberate.")}</p>
      </div>

      {error !== null && <ResourceError message={errorMessage(error, t)} />}

      {confirming ? (
        // Cancelling voids every approval collected so far — votes are not carried over
        // when a request is reopened (policy 12), so this is not the reversible click its
        // single ghost button made it look like.
        <div className="flex flex-col gap-3 rounded-lg border border-accent-error/40 bg-accent-error/10 p-3.5">
          <p className="text-sm leading-relaxed text-ink">{t("consilium.payout.cancelWarning", "Cancelling voids every approval collected so far. Votes are not carried over if the request is reopened, so the owners who have already answered would each have to answer again.", { approvals })}</p>
          <div className="flex flex-col gap-2.5 sm:flex-row">
            <Button variant="destructive" size="sm" disabled={busy} onClick={() => void cancel()}>
              {busy && <Loader2 className="size-4 animate-spin" />}
              {t("consilium.payout.cancelConfirm", "Cancel and void the votes")}
            </Button>
            <Button variant="ghost" size="sm" disabled={busy} onClick={() => setConfirming(false)}>
              {t("ui.cancel", "Cancel")}
            </Button>
          </div>
        </div>
      ) : (
        <Button variant="ghost" size="sm" className="self-start" onClick={() => setConfirming(true)}>
          {t("consilium.payout.cancel", "Cancel this request")}
        </Button>
      )}
    </div>
  );
}

/**
 * Stand down as an owner.
 *
 * No consilium — nobody has to agree to let you leave — but the same floor applies, and the
 * confirmation is typing your own address rather than pressing a second button. That is the
 * `confirm_email` the BFF asks for, and its only job is to make this impossible to do by
 * accident, which is why what the reader typed is what gets sent.
 */
function ResignCard({ email, loadingProfile }: { email: string; loadingProfile: boolean }) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [done, setDone] = useState(false);

  const matches = email.length > 0 && typed.trim().toLowerCase() === email.toLowerCase();

  const submit = async () => {
    if (busy || !matches) return;
    setBusy(true);
    setError(null);
    try {
      await resignOwnership(typed.trim());
      setDone(true);
      setOpen(false);
      setTyped("");
    } catch (cause) {
      setError(cause);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t("consilium.resign.title", "Stand down as an owner")}</CardTitle>
        <CardDescription className="text-balance">{t("consilium.resign.sub", "Give up your own seat. Nobody has to agree to let you leave.")}</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-3.5">
        {done ? (
          <p className="text-sm text-positive" role="status">
            {t("consilium.resign.done", "You are no longer an owner.")}
          </p>
        ) : loadingProfile ? (
          <Skeleton className="h-9 w-40" />
        ) : email.length === 0 ? (
          // Without an email there is nothing to type and nothing to compare, so the button
          // would sit permanently disabled with no way to find out why.
          <p className="text-sm text-ink-soft">{t("consilium.resign.unavailable", "We couldn't load your account, so this can't be confirmed right now. Reload the page and try again.")}</p>
        ) : open ? (
          <>
            <p className="text-sm leading-relaxed text-ink-soft">{t("consilium.resign.warning", "Your ownership ends as soon as this is recorded. You would need the remaining owners to make you an owner again. Nothing else about your account changes.")}</p>
            <p className="text-xs text-ink-soft">{t("consilium.resign.floorWarning", "A fund needs at least three owners to authorise a payout. If standing down would leave fewer than three, it is refused.")}</p>
            <div className="flex flex-col gap-2">
              <Label htmlFor="resign-confirm">{t("consilium.resign.confirmLabel", "Type {email} to confirm", { email })}</Label>
              <Input
                id="resign-confirm"
                value={typed}
                onChange={(e) => setTyped(e.target.value)}
                autoComplete="off"
                autoCapitalize="none"
                inputMode="email"
                spellCheck={false}
                placeholder={email}
              />
            </div>
            {error !== null && <ResourceError message={errorMessage(error, t)} />}
            <div className="flex flex-col gap-2.5 sm:flex-row">
              <Button variant="destructive" className="sm:flex-1" disabled={!matches || busy} onClick={() => void submit()}>
                {busy && <Loader2 className="size-4 animate-spin" />}
                {t("consilium.resign.confirm", "Stand down as an owner")}
              </Button>
              <Button
                variant="ghost"
                disabled={busy}
                onClick={() => {
                  setOpen(false);
                  setTyped("");
                  setError(null);
                }}
              >
                {t("ui.cancel", "Cancel")}
              </Button>
            </div>
          </>
        ) : (
          <Button variant="outline" className="self-start" onClick={() => setOpen(true)}>
            {t("consilium.resign.start", "Stand down")}
          </Button>
        )}
      </CardContent>
    </Card>
  );
}
