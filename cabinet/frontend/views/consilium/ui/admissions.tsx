"use client";

// Open owner admissions: what each one says, where the caller stands in it, and the form
// that opens a new one.
//
// The sibling of `./removals.tsx`, and read alongside it — the layout, the branches and the
// zero states are deliberately the same shape, because these are the two halves of one
// question and an owner should not have to relearn the screen halfway down it. What is NOT
// the same is the rule and the words, and both differences are load-bearing:
//
//   · **The rule is stricter.** A removal has two paths — the target accepts, or the
//     eligible peers are unanimous. An admission has one: unanimity of every owner except
//     the initiator, with at least one such peer. A reader who has just learned the removal
//     rule will assume a majority is enough here, so the copy says otherwise in the tally
//     line itself rather than in a footnote.
//   · **The verbs are different.** `admit`/`reject`, never `remove`/`keep`. The plane keeps
//     two enums for this, and the one word they share means opposite things — `reject`
//     keeps an owner on a removal and refuses a candidate here. Nothing in this file is
//     shared with the removal controls; the two vote buttons post through
//     `voteOnAdmission`, whose parameter type accepts neither of the other plane's words.
//
// Why admission is governed at all, if a reader wonders: `SetRole` granting `owner` freely
// would let a bad actor mint sock puppets before opening a payout and then reach quorum
// legitimately, which would make every other control in this feature decorative
// (docs/CONSILIUM.md, policy 21).

import { Loader2, UserPlus, Users } from "lucide-react";
import { Fragment, useState } from "react";

import { useLocale, useT } from "@evinvest/i18n/react";
import {
  Badge,
  Button,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
  Field,
  FieldDescription,
  FieldGroup,
  FieldLabel,
  Input,
  Item,
  ItemContent,
  ItemGroup,
  ItemSeparator,
  ItemTitle,
  Separator,
  Textarea,
} from "@evinvest/uikit";

import { cancelAdmission, proposeAdmission, voteOnAdmission } from "@/entities/governance/model/governance-resource";
import type { AdmissionVote, OwnerAdmission, OwnerList } from "@/shared/contracts/governance";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { expiresIn, formatMoment } from "@/shared/lib/datetime";
import { Settled } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import {
  admissionTally,
  admissionVoteLabel,
  admissionVoteTone,
  isSettled,
  standingInAdmission,
  stateLabel,
  stateTone,
} from "@/views/consilium/lib/format";
import { ownerCount, type Read } from "@/views/consilium/lib/reads";
import { AdmissionsSkeleton, ProposeSkeleton } from "@/views/consilium/ui/loading";
import { ProposalList } from "@/views/consilium/ui/proposal-list";
import { ReadFailure } from "@/views/consilium/ui/read-failure";

/**
 * The admissions section.
 *
 * The card and its four read states are `ProposalList`'s; the unanimity rule and the
 * admit/reject ballot are `AdmissionCard`'s, and stay there. The shell is handed a render
 * function precisely so that no vote type crosses into shared code.
 */
export function AdmissionList({
  read,
  userId,
  onRetry,
  retrying,
}: {
  read: Read<OwnerAdmission[]>;
  userId: string | null;
  onRetry: () => void;
  retrying: boolean;
}) {
  const t = useT();
  return (
    <ProposalList
      read={read}
      title={t("consilium.admissions.title")}
      description={t("consilium.admissions.sub")}
      skeleton={<AdmissionsSkeleton />}
      failedTitle={t("consilium.admissions.failedTitle")}
      failedBody={t("consilium.admissions.failedBody")}
      emptyIcon={<UserPlus />}
      emptyTitle={t("consilium.admissions.emptyTitle")}
      emptyBody={t("consilium.admissions.emptyBody")}
      onRetry={onRetry}
      retrying={retrying}
      itemKey={(admission) => admission.id}
    >
      {(admission) => <AdmissionCard admission={admission} userId={userId} />}
    </ProposalList>
  );
}

function AdmissionCard({ admission, userId }: { admission: OwnerAdmission; userId: string | null }) {
  const t = useT();
  const locale = useLocale();
  const [busy, setBusy] = useState<AdmissionVote | "cancel" | null>(null);
  const [error, setError] = useState<unknown>(null);

  const settled = isSettled(admission.state, admission.decided_at);
  const standing = standingInAdmission(admission, userId);
  const peers = admission.peers ?? [];
  const tally = admissionTally(peers);

  const act = async (what: AdmissionVote | "cancel") => {
    if (busy) return;
    setBusy(what);
    setError(null);
    try {
      if (what === "cancel") await cancelAdmission(admission.id);
      else await voteOnAdmission(admission.id, what);
    } catch (cause) {
      setError(cause);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-wrap items-start justify-between gap-x-3 gap-y-1.5">
        <div className="flex min-w-0 flex-col gap-1">
          <p className="text-base font-semibold text-ink">
            {t("consilium.admission.heading", { candidate: admission.candidate_email || admission.candidate_user_id })}
          </p>
          <p className="text-sm text-ink-soft">
            {t("consilium.admission.openedBy", { initiator: admission.initiator_email, at: formatMoment(admission.created_at, locale) })}
          </p>
        </div>
        <Badge variant="outline" className={cn("shrink-0", stateTone(admission.state))}>
          {stateLabel(admission.state, t)}
        </Badge>
      </div>

      <div className="flex flex-col gap-5">
        <div className="flex flex-col gap-1.5">
          <span className="text-xs font-medium uppercase tracking-wider text-ink-soft">{t("consilium.admission.reason")}</span>
          <p className="whitespace-pre-line rounded-lg bg-secondary px-3.5 py-3 text-sm leading-relaxed text-ink">
            {admission.reason?.trim() || t("consilium.admission.noReason")}
          </p>
        </div>

        <div className="flex flex-col gap-2.5">
          <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
            {/* Written as "x of y agreed", with the rule named on the same line. A reader
                arriving from the removal card above has just learned a rule with a second
                path through the target's mailbox, and will read a partial count here as
                progress toward a majority. There is no majority here and no second path:
                one reject ends it. */}
            <span className="text-sm font-medium tabular-nums text-ink">
              {t("consilium.admission.peerTally", { admitted: tally.toAdmit, total: tally.total })}
            </span>
            <span className="text-xs text-ink-soft">{t("consilium.admission.unanimityNote")}</span>
          </div>

          {tally.total === 0 ? (
            // The plane refuses to open an admission with nobody to agree, so this is a
            // proposal that has lost its peers since — not one waiting on a mailbox. There
            // is no second path to fall back on, unlike a removal.
            <p className="text-xs text-accent-warn">{t("consilium.admission.noPeers")}</p>
          ) : (
            <ItemGroup>
              {peers.map((peer, i) => (
                <Fragment key={peer.user_id}>
                  {i > 0 && <ItemSeparator />}
                  <Item size="sm" className="px-0">
                    <ItemContent className="min-w-0 gap-0.5">
                      <ItemTitle className="block w-auto truncate font-medium">{peer.email}</ItemTitle>
                    </ItemContent>
                    <div className={cn("shrink-0 text-xs font-semibold", admissionVoteTone(peer.vote))}>
                      {admissionVoteLabel(peer.vote, t)}
                    </div>
                  </Item>
                </Fragment>
              ))}
            </ItemGroup>
          )}

          {/* Only once someone has actually refused. Said while every peer is still to
              answer it would read as a warning about a proposal that is going fine. */}
          {tally.toReject > 0 && !settled && <p className="text-xs text-accent-error">{t("consilium.admission.oneRejectEnds")}</p>}
        </div>

        {!settled && (
          <p className="text-xs tabular-nums text-ink-soft">
            {t("consilium.admission.expires", { at: formatMoment(admission.expires_at, locale), left: expiresIn(admission.expires_at, t) })}
          </p>
        )}

        {error !== null && <ResourceError message={errorMessage(error, t)} />}

        {!settled && (
          <>
            <Separator />
            {standing.role === "peer" && standing.vote === null ? (
              // Two equal-weight controls, told apart by colour alone — the same call the
              // removal card makes, for the same reason: the reader is being asked to
              // judge, and a heavier button would be the design taking a side.
              <div className="flex flex-col gap-2.5 sm:flex-row">
                <Button
                  variant="outline"
                  className="border-accent-error/40 text-accent-error hover:bg-accent-error/10 hover:text-accent-error sm:flex-1"
                  disabled={busy !== null}
                  onClick={() => void act("reject")}
                >
                  {busy === "reject" && <Loader2 className="size-4 animate-spin" />}
                  {t("consilium.admission.voteReject")}
                </Button>
                <Button variant="outline" className="sm:flex-1" disabled={busy !== null} onClick={() => void act("admit")}>
                  {busy === "admit" && <Loader2 className="size-4 animate-spin" />}
                  {t("consilium.admission.voteAdmit")}
                </Button>
              </div>
            ) : (
              <p className="text-sm leading-relaxed text-ink-soft">
                <WhyNoVote standing={standing} admission={admission} />
              </p>
            )}

            {standing.role === "initiator" && (
              <Button variant="ghost" size="sm" className="self-start" disabled={busy !== null} onClick={() => void act("cancel")}>
                {busy === "cancel" && <Loader2 className="size-4 animate-spin" />}
                {t("consilium.admission.cancel")}
              </Button>
            )}
          </>
        )}
      </div>
    </div>
  );
}

/**
 * Why this reader has no vote here.
 *
 * Same contract as the removal card's: name the rule AND the abuse it closes, because a
 * reader who is told the reason is not being told off. The initiator's case is the one that
 * matters most — they are looking at their own proposal, and "you opened this, so you do
 * not vote on it; the bar you have to clear is everyone else agreeing" is a design they can
 * agree with, where a bare "you cannot vote" reads as the page malfunctioning.
 */
function WhyNoVote({ standing, admission }: { standing: ReturnType<typeof standingInAdmission>; admission: OwnerAdmission }) {
  const t = useT();
  if (standing.role === "peer") return <>{t("consilium.admission.youVoted", { vote: admissionVoteLabel(standing.vote, t) })}</>;
  if (standing.role === "candidate") return <>{t("consilium.admission.youAreCandidate")}</>;
  if (standing.role === "initiator") {
    return <>{t("consilium.admission.youOpened", { candidate: admission.candidate_email || admission.candidate_user_id })}</>;
  }
  return <>{t("consilium.admission.notAVoter")}</>;
}

/**
 * Propose admitting someone.
 *
 * Two things this does NOT do, both on the precedent the removal form set:
 *
 *   · It does not re-derive the server's refusals. The plane owns "may this open?" — the
 *     genesis window, the peer set, whether the candidate is already seated — and a client
 *     that re-computes those is a client that can disagree, in the direction of blocking a
 *     legitimate action. The rules are stated; the button stays live; the server's own
 *     refusal is what appears.
 *   · It does not offer a picker, and the raw id field is the considered answer rather
 *     than an unfinished one. A candidate is by definition not an owner, so the roster
 *     cannot list them; the only read that enumerates non-owners is the operator console's
 *     `/api/admin/users`, which is gated on operator/admin. An owner who is not also an
 *     operator — the ordinary case, and exactly the person this form exists for — is
 *     refused it. A searchable dropdown would therefore be empty or forbidden for half the
 *     people entitled to use this form, and swapping a field that always works for a
 *     control that sometimes does is not an improvement. Closing this properly needs an
 *     owner-readable candidate lookup on the plane, which is a contract change, not a
 *     component change. Until then the field takes the id and the description says where
 *     to find it.
 */
export function ProposeAdmission({
  roster,
  onRetry,
  retrying,
}: {
  roster: Read<OwnerList>;
  onRetry: () => void;
  retrying: boolean;
}) {
  const t = useT();
  const [candidate, setCandidate] = useState("");
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [opened, setOpened] = useState(false);

  // `null` when the roster has not landed, and the genesis note below is gated on it: the
  // note is a statement about how many owners this fund has, so it may not be printed off
  // a read that did not arrive.
  const seated = ownerCount(roster);

  const submit = async () => {
    if (busy || candidate.trim().length === 0 || reason.trim().length === 0) return;
    setBusy(true);
    setError(null);
    setOpened(false);
    try {
      await proposeAdmission(candidate.trim(), reason.trim());
      setCandidate("");
      setReason("");
      setOpened(true);
    } catch (cause) {
      setError(cause);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t("consilium.admit.title")}</CardTitle>
        <CardDescription className="text-balance">{t("consilium.admit.sub")}</CardDescription>
      </CardHeader>
      <CardContent>
        <Settled className="flex flex-col gap-4" loading={roster.status === "loading"} skeleton={<ProposeSkeleton />}>
          {roster.status === "loading" ? null : roster.status === "failed" ? (
            <ReadFailure
              title={t("consilium.admit.unknownTitle")}
              body={t("consilium.admit.unknownBody")}
              onRetry={onRetry}
              retrying={retrying}
            />
          ) : (
            <FieldGroup className="gap-4">
              {/* The bootstrap, stated only when the roster says it applies. An admission
                  needs at least one owner besides whoever opens it, so it cannot seat the
                  founders — that is the genesis seed's job, written by the service at
                  start-up and only while the register is empty. There is no longer a
                  direct-grant route beside it at any roster size: `SetRole` refuses
                  `owner` unconditionally (docs/CONSILIUM.md, § Genesis, policy 21). */}
              {seated !== null && seated < 2 && (
                <p className="rounded-lg border border-accent-warn/40 bg-accent-warn/10 px-3.5 py-3 text-sm leading-relaxed text-ink">
                  {t("consilium.admit.genesis", { n: seated })}
                </p>
              )}

              <Field>
                <FieldLabel htmlFor="admission-candidate">{t("consilium.admit.candidateLabel")}</FieldLabel>
                <Input
                  id="admission-candidate"
                  value={candidate}
                  onChange={(e) => {
                    setCandidate(e.target.value);
                    setOpened(false);
                  }}
                  autoComplete="off"
                  autoCapitalize="none"
                  spellCheck={false}
                  className="font-mono-tech"
                  placeholder={t("consilium.admit.candidatePlaceholder")}
                />
                <FieldDescription>{t("consilium.admit.candidateHint")}</FieldDescription>
              </Field>

              <Field>
                <FieldLabel htmlFor="admission-reason">{t("consilium.admit.reasonLabel")}</FieldLabel>
                <Textarea
                  id="admission-reason"
                  value={reason}
                  onChange={(e) => {
                    setReason(e.target.value);
                    setOpened(false);
                  }}
                  rows={3}
                  maxLength={1000}
                  placeholder={t("consilium.admit.reasonPlaceholder")}
                />
                <FieldDescription>{t("consilium.admit.reasonHint")}</FieldDescription>
              </Field>

              {/* Stated unconditionally, because it is the rule rather than a verdict about
                  this roster — and because it is STRICTER than the removal rule a reader
                  has just met further up the page. Assuming a majority is enough is the
                  natural mistake, so it is the one the copy pre-empts. */}
              <p className="text-xs text-ink-soft">{t("consilium.admit.unanimityWarning")}</p>

              {error !== null && <ResourceError message={errorMessage(error, t)} />}
              {opened && (
                <p className="text-sm text-positive" role="status">
                  {t("consilium.admit.opened")}
                </p>
              )}

              <Button
                variant="outline"
                className="self-start"
                disabled={busy || candidate.trim().length === 0 || reason.trim().length === 0}
                onClick={() => void submit()}
              >
                {busy ? <Loader2 className="size-4 animate-spin" /> : <Users className="size-4" />}
                {t("consilium.admit.submit")}
              </Button>
            </FieldGroup>
          )}
        </Settled>
      </CardContent>
    </Card>
  );
}
