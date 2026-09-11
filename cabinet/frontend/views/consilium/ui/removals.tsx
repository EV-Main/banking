"use client";

// Open owner removals: what each one says, where the caller stands in it, and the form that
// opens a new one.
//
// The hardest thing on this screen is not the voting — it is explaining, without sounding
// like a refusal, why someone is looking at a request they cannot answer. Three people are
// in that position for every removal: the target (whose answer belongs in their own
// mailbox), the initiator (who gets no vote by design), and any owner added after the
// request was opened (who was not in the frozen voter set). Each gets a sentence saying
// what the rule is and why it exists, because every one of those rules is there to stop a
// particular abuse, and a reader who is told the reason is not being told off.

import { Loader2, ScrollText, UserMinus } from "lucide-react";
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
  Empty,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
  Item,
  ItemContent,
  ItemGroup,
  ItemSeparator,
  ItemTitle,
  Label,
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
  Separator,
  Textarea,
} from "@evinvest/uikit";

import { cancelRemoval, proposeRemoval, voteOnRemoval } from "@/entities/governance/model/governance-resource";
import type { OwnerList, OwnerRemoval, RemovalVote } from "@/shared/contracts/governance";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { ResourceError } from "@/shared/ui/resource-error";
import { Settled } from "@/shared/ui/motion";
import { expiresIn, formatMoment } from "@/shared/lib/datetime";
import {
  EMPTY_BOX,
  isSettled,
  peerTally,
  standingIn,
  stateLabel,
  stateTone,
  targetAnswer,
  voteLabel,
  voteTone,
} from "@/views/consilium/lib/format";
import { removalCandidates, type Read } from "@/views/consilium/lib/reads";
import { ProposeSkeleton, RemovalsSkeleton } from "@/views/consilium/ui/loading";
import { ProposalList } from "@/views/consilium/ui/proposal-list";
import { ReadFailure } from "@/views/consilium/ui/read-failure";

/**
 * The removals section.
 *
 * The card, its four read states and the separators between proposals are
 * `ProposalList`'s; what belongs to removals is the copy and `RemovalCard` — which is where
 * the two-path rule and the remove/keep ballot stay, well away from anything admissions
 * share.
 */
export function RemovalList({
  read,
  userId,
  onRetry,
  retrying,
}: {
  read: Read<OwnerRemoval[]>;
  userId: string | null;
  onRetry: () => void;
  retrying: boolean;
}) {
  const t = useT();
  return (
    <ProposalList
      read={read}
      title={t("consilium.removals.title")}
      description={t("consilium.removals.sub")}
      skeleton={<RemovalsSkeleton />}
      failedTitle={t("consilium.removals.failedTitle")}
      failedBody={t("consilium.removals.failedBody")}
      emptyIcon={<ScrollText />}
      emptyTitle={t("consilium.removals.emptyTitle")}
      emptyBody={t("consilium.removals.emptyBody")}
      onRetry={onRetry}
      retrying={retrying}
      itemKey={(removal) => removal.id}
    >
      {(removal) => <RemovalCard removal={removal} userId={userId} />}
    </ProposalList>
  );
}

function RemovalCard({ removal, userId }: { removal: OwnerRemoval; userId: string | null }) {
  const t = useT();
  const locale = useLocale();
  const [busy, setBusy] = useState<RemovalVote | "cancel" | null>(null);
  const [error, setError] = useState<unknown>(null);

  const settled = isSettled(removal.state, removal.decided_at);
  const standing = standingIn(removal, userId);
  // See `peerTally` on why the list is read defensively: proto3 JSON omits an empty one.
  const peers = removal.peers ?? [];
  const tally = peerTally(peers);

  const act = async (what: RemovalVote | "cancel") => {
    if (busy) return;
    setBusy(what);
    setError(null);
    try {
      if (what === "cancel") await cancelRemoval(removal.id);
      else await voteOnRemoval(removal.id, what);
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
          <p className="text-base font-semibold text-ink">{t("consilium.removal.heading", { target: removal.target_email })}</p>
          <p className="text-sm text-ink-soft">
            {t("consilium.removal.openedBy", { initiator: removal.initiator_email, at: formatMoment(removal.created_at, locale) })}
          </p>
        </div>
        <Badge variant="outline" className={cn("shrink-0", stateTone(removal.state))}>
          {stateLabel(removal.state, t)}
        </Badge>
      </div>

      <div className="flex flex-col gap-5">
        <div className="flex flex-col gap-1.5">
          <span className="text-xs font-medium uppercase tracking-wider text-ink-soft">{t("consilium.removal.reason")}</span>
          <p className="whitespace-pre-line rounded-lg bg-secondary px-3.5 py-3 text-sm leading-relaxed text-ink">
            {removal.reason?.trim() || t("consilium.removal.noReason")}
          </p>
        </div>

        {/* The target's own answer is not one vote among the peers' — it is the other,
            independent way this can carry, so it is stated on its own line rather than
            folded into the tally below. */}
        <TargetAnswer removal={removal} />

        <div className="flex flex-col gap-2.5">
          <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
            <span className="text-sm font-medium tabular-nums text-ink">
              {t("consilium.removal.peerTally", { removed: tally.toRemove, total: tally.total })}
            </span>
            <span className="text-xs text-ink-soft">{t("consilium.removal.unanimityNote")}</span>
          </div>

          {tally.total === 0 ? (
            // With two owners the eligible set is empty, and "everyone in an empty set
            // agreed" would let either owner expel the other. The rule requires at least one
            // peer voter, so this removal can only ever carry on the target's own answer.
            <p className="text-xs text-accent-warn">{t("consilium.removal.noPeers")}</p>
          ) : (
            <ItemGroup>
              {peers.map((peer, i) => (
                <Fragment key={peer.user_id}>
                  {i > 0 && <ItemSeparator />}
                  <Item size="sm" className="px-0">
                    <ItemContent className="min-w-0 gap-0.5">
                      <ItemTitle className="block w-auto truncate font-medium">{peer.email}</ItemTitle>
                    </ItemContent>
                    <div className={cn("shrink-0 text-xs font-semibold", voteTone(peer.vote))}>{voteLabel(peer.vote, t)}</div>
                  </Item>
                </Fragment>
              ))}
            </ItemGroup>
          )}

        </div>

        {!settled && (
          <p className="text-xs text-ink-soft tabular-nums">
            {t("consilium.removal.expires", { at: formatMoment(removal.expires_at, locale), left: expiresIn(removal.expires_at, t) })}
          </p>
        )}

        {error !== null && <ResourceError message={errorMessage(error, t)} />}

        {!settled && (
          <>
            <Separator />
            {standing.role === "peer" && standing.vote === null ? (
              // Two equal-weight controls, told apart by colour alone — deliberately, and
              // unlike the emailed approval page. There the reader is being asked to
              // authorize something and one answer is the act; here they are being asked to
              // judge, and putting a thumb on either scale by making one button heavier
              // would be the design taking a side in someone's removal.
              <div className="flex flex-col gap-2.5 sm:flex-row">
                <Button variant="outline" className="sm:flex-1" disabled={busy !== null} onClick={() => void act("keep")}>
                  {busy === "keep" && <Loader2 className="size-4 animate-spin" />}
                  {t("consilium.removal.voteKeep")}
                </Button>
                <Button
                  variant="outline"
                  className="border-accent-error/40 text-accent-error hover:bg-accent-error/10 hover:text-accent-error sm:flex-1"
                  disabled={busy !== null}
                  onClick={() => void act("remove")}
                >
                  {busy === "remove" && <Loader2 className="size-4 animate-spin" />}
                  {t("consilium.removal.voteRemove")}
                </Button>
              </div>
            ) : (
              <p className="text-sm leading-relaxed text-ink-soft">
                <WhyNoVote standing={standing} removal={removal} />
              </p>
            )}

            {standing.role === "initiator" && (
              <Button variant="ghost" size="sm" className="self-start" disabled={busy !== null} onClick={() => void act("cancel")}>
                {busy === "cancel" && <Loader2 className="size-4 animate-spin" />}
                {t("consilium.removal.cancel")}
              </Button>
            )}
          </>
        )}
      </div>
    </div>
  );
}

function TargetAnswer({ removal }: { removal: OwnerRemoval }) {
  const t = useT();
  const locale = useLocale();
  // Normalised, not compared raw: an unanswered target arrives as "" or "pending".
  const answer = targetAnswer(removal);
  if (answer === "remove") {
    return (
      <p className="text-sm text-positive">
        {t("consilium.removal.targetAccepted", { target: removal.target_email, at: formatMoment(removal.target_decided_at ?? undefined, locale) })}
      </p>
    );
  }
  if (answer === "keep") {
    return <p className="text-sm text-accent-warn">{t("consilium.removal.targetRefused", { target: removal.target_email })}</p>;
  }
  return (
    <p className="text-sm text-ink-soft">
      {t(removal.target_notified ? "consilium.removal.targetPending" : "consilium.removal.targetNotYetTold", { target: removal.target_email })}
    </p>
  );
}

/**
 * Why this reader has no vote here.
 *
 * Every branch names the rule AND the abuse it closes. "You can't vote on this" is a
 * refusal; "the initiator is counted in the threshold but doesn't vote, so opening a
 * request can't lower the bar it has to clear" is an explanation, and it happens to be the
 * actual reason (docs/CONSILIUM.md § Quorum arithmetic).
 */
function WhyNoVote({ standing, removal }: { standing: ReturnType<typeof standingIn>; removal: OwnerRemoval }) {
  const t = useT();
  if (standing.role === "peer") return <>{t("consilium.removal.youVoted", { vote: voteLabel(standing.vote, t) })}</>;
  if (standing.role === "target") return <>{t("consilium.removal.youAreTarget")}</>;
  if (standing.role === "initiator") return <>{t("consilium.removal.youOpened", { target: removal.target_email })}</>;
  return <>{t("consilium.removal.notAVoter")}</>;
}

/**
 * Open a removal.
 *
 * The floor is stated rather than enforced here. The BFF refuses a removal that would leave
 * fewer than three owners, and it is the authority — re-implementing that arithmetic in the
 * browser would mean a client that disagrees with the server is a client that blocks a
 * legitimate action. So the note explains what will happen; the button stays live and the
 * server's own refusal is what appears if it does.
 */
export function ProposeRemoval({
  roster,
  userId,
  onRetry,
  retrying,
}: {
  roster: Read<OwnerList>;
  userId: string | null;
  onRetry: () => void;
  retrying: boolean;
}) {
  const t = useT();
  const [target, setTarget] = useState("");
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [opened, setOpened] = useState(false);

  // `null` and `[]` are different answers and this card renders them differently. `null` is
  // "the roster did not load, so the page has no idea who is in this fund"; `[]` is "the
  // roster loaded and you really are the only owner in it". Collapsing the first into the
  // second is what put "There is nobody to propose — you are the only owner listed" on
  // screen while three reads were answering 404.
  const candidates = removalCandidates(roster, userId);

  const submit = async () => {
    if (busy || !target || reason.trim().length === 0) return;
    setBusy(true);
    setError(null);
    setOpened(false);
    try {
      await proposeRemoval(target, reason.trim());
      setTarget("");
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
        <CardTitle className="text-base">{t("consilium.propose.title")}</CardTitle>
        <CardDescription className="text-balance">{t("consilium.propose.sub")}</CardDescription>
      </CardHeader>
      <CardContent>
        <Settled
          className="flex flex-col gap-4"
          loading={roster.status === "loading"}
          skeleton={<ProposeSkeleton />}
        >
          {roster.status === "loading" ? null : candidates === null ? (
            <ReadFailure
              title={t("consilium.propose.unknownTitle")}
              body={t("consilium.propose.unknownBody")}
              onRetry={onRetry}
              retrying={retrying}
            />
          ) : candidates.length === 0 ? (
            <Empty className={EMPTY_BOX}>
              <EmptyHeader>
                <EmptyMedia variant="icon">
                  <UserMinus />
                </EmptyMedia>
                <EmptyTitle>{t("consilium.propose.noneTitle")}</EmptyTitle>
                <EmptyDescription>{t("consilium.propose.noneBody")}</EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : (
            <>
              <div className="flex flex-col gap-2">
                <Label htmlFor="removal-target">{t("consilium.propose.targetLabel")}</Label>
                <Select
                  value={target}
                  onValueChange={(next) => {
                    setTarget(next);
                    setOpened(false);
                  }}
                >
                  <SelectTrigger id="removal-target" className="w-full">
                    <SelectValue placeholder={t("consilium.propose.targetPlaceholder")} />
                  </SelectTrigger>
                  <SelectContent>
                    {candidates.map((owner) => (
                      <SelectItem key={owner.user_id} value={owner.user_id}>
                        {owner.display_name ? `${owner.display_name} · ${owner.email}` : owner.email}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>

              <div className="flex flex-col gap-2">
                <Label htmlFor="removal-reason">{t("consilium.propose.reasonLabel")}</Label>
                <Textarea
                  id="removal-reason"
                  value={reason}
                  onChange={(e) => {
                    setReason(e.target.value);
                    setOpened(false);
                  }}
                  rows={3}
                  maxLength={1000}
                  placeholder={t("consilium.propose.reasonPlaceholder")}
                />
                {/* The reason is emailed to the person it is about, in these words. Saying so
                    before it is written is worth more than any amount of moderation after. */}
                <p className="text-xs text-ink-soft">{t("consilium.propose.reasonHint")}</p>
              </div>

              {/* Stated unconditionally, because it is the rule rather than a verdict about
                  this roster. Re-deriving "would this breach the floor?" in the browser means
                  a client that can disagree with the server — and the direction it would be
                  wrong in is blocking a removal the fund is entitled to make. */}
              <p className="text-xs text-ink-soft">{t("consilium.propose.floorWarning")}</p>

              {error !== null && <ResourceError message={errorMessage(error, t)} />}
              {opened && (
                <p className="text-sm text-positive" role="status">
                  {t("consilium.propose.opened")}
                </p>
              )}

              <Button
                variant="outline"
                className="self-start border-accent-error/40 text-accent-error hover:bg-accent-error/10 hover:text-accent-error"
                disabled={busy || !target || reason.trim().length === 0}
                onClick={() => void submit()}
              >
                {busy && <Loader2 className="size-4 animate-spin" />}
                {t("consilium.propose.submit")}
              </Button>
            </>
          )}
        </Settled>
      </CardContent>
    </Card>
  );
}
