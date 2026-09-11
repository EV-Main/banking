"use client";

import { useT } from "@evinvest/i18n/react";

import { Loader2 } from "lucide-react";
import { type ReactNode, useState } from "react";

import { Button } from "@evinvest/uikit";

import { startVerification, type KycStart } from "@/features/kyc/api/kyc-client";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { Hairline, RowLabel } from "@/shared/ui/list-card";

// The one place in the cabinet a user can begin identity verification themselves. It sits
// inside `card-Verification`, below the rows that state what the hub currently holds, so
// the card reads as status-then-action rather than as a form.

type State = { kind: "idle" | "starting" } | Exclude<KycStart, { kind: "started" }>;

export function StartVerificationRow() {
  const t = useT();
  const [state, setState] = useState<State>({ kind: "idle" });
  const starting = state.kind === "starting";

  async function begin() {
    setState({ kind: "starting" });
    const result = await startVerification();
    if (result.kind === "started") {
      // Deliberately NOT back to idle first: the provider's page is already loading over
      // this one, and re-enabling the button would flash a second chance at someone who
      // is leaving — and buy a duplicate case if they took it.
      window.location.assign(result.redirectUrl);
      return;
    }
    setState(result);
  }

  return (
    <>
      <Hairline />
      <div className="flex flex-col py-3.5">
        <div className="flex min-w-0 items-center justify-between gap-3">
          <RowLabel title={t("profile.kyc.startTitle")} sub={t("profile.kyc.startSub")} />
          {/* i18n-max: 12 — a `shrink-0` Button beside the `min-w-0` row label. */}
          <Button type="button" size="sm" onClick={begin} disabled={starting} aria-busy={starting} className="rounded-lg font-semibold">
            {starting && <Loader2 className="size-4 animate-spin" aria-hidden />}
            {t("profile.kyc.start")}
          </Button>
        </div>
        {/* The live region is mounted empty and stays mounted: a `role="status"` inserted
            in the same tick as its own text is unreliably announced. It carries no gap of
            its own so an empty one takes no space — the message brings its own margin. */}
        <div role="status">
          <Outcome state={state} />
        </div>
      </div>
    </>
  );
}

function Outcome({ state }: { state: State }) {
  const t = useT();
  // The vendor's actual trouble — no balance, no configuration, an outage — is ours to fix
  // and never reaches the browser, so this says only that it cannot run and offers the one
  // address that can help. The address is its own key rather than interpolated into the
  // sentence: `Translate` returns a string, so a link cannot be handed to it as a value.
  if (state.kind === "unavailable") {
    return (
      <Note>
        {t("profile.kyc.unavailable")}
        {state.contact ? (
          <>
            {" "}
            <a
              href={`mailto:${state.contact}`}
              className="font-medium text-accent-debug underline underline-offset-2 outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              {t("profile.kyc.contact", { contact: state.contact })}
            </a>
          </>
        ) : null}
      </Note>
    );
  }
  // Not the user's doing and not ours to fix in this session — stated calmly, like the 503.
  if (state.kind === "throttled") return <Note>{t("profile.kyc.tooManyAttempts")}</Note>;
  if (state.kind === "stale") return <Note tone="destructive">{t("err.csrf")}</Note>;
  if (state.kind === "failed") return <Note tone="destructive">{errorMessage(state.error, t)}</Note>;
  return null;
}

function Note({ tone, children }: { tone?: "destructive"; children: ReactNode }) {
  return (
    <p className={cn("mt-2 text-xs leading-snug", tone === "destructive" ? "text-accent-error" : "text-ink-soft")}>
      {children}
    </p>
  );
}
