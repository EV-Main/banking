"use client";

// The caller's verification state, as the identity plane holds it: their tier, and the
// attempt that is running right now if there is one.
//
// This is the resource that closes #190. Before it, the only input to "offer Start?" was
// `kyc_level`, which is precisely the value that lags while a case is running — so a user
// coming back from the vendor before the webhook landed was shown the same Start button they
// had just pressed, and a second paid vendor session was one click away.
//
// It does NOT replace the poll on `entities/user/model/profile-resource` (#219), and both
// comments now say so. That loop watched `kyc_level` move off 0 as a stand-in for "the verdict
// arrived"; the verdict has a name here now, so what is left there is the narrower job the
// mirrored tier alone can do — waiting for the bridge to catch up, for the account chip and
// the level pill that read the profile and not this. On a tier-0 profile tab the two loops do
// run together; `use-kyc-status` shortens that by invalidating the profile the moment the two
// levels disagree, but the profile's own loop is what closes the gap for certain, including on
// every screen this hook is not mounted on.

import { fetchKycStatus } from "@/features/kyc/api/kyc-client";
import { defineResource } from "@/shared/lib/resource";

export const kycStatusResource = defineResource({
  name: "kyc.status",
  fetch: fetchKycStatus,
  // Short: a verdict landing while the page sits open changes what the screen may offer, and
  // a stale "no case" is the one that costs money.
  revalidate: 15,
  poll: {
    // Three states are worth another look, and a verified caller with nothing running is not
    // one of them:
    //   · a running case — the verdict can land at any moment, and nothing else announces it;
    //   · tier 0 with no case — an operator can still raise the tier by hand, which is the
    //     path this cabinet is actually on today (see `@/shared/config/support`);
    //   · `null` — and `null` now means ONE thing, which is what makes polling on it safe:
    //     a 404, i.e. a concierge that has not shipped the route (#75). Everything else —
    //     5xx, a network failure, an expired session — is re-raised by `fetchKycStatus`
    //     rather than flattened into it, so it lands in `error` here and stops the loop
    //     instead of re-asking a broken plane once a minute until the tab closes. Re-asking
    //     on a 404 is what lets a tab open across the deployment pick the route up.
    while: (status) => status === null || status?.case != null || status?.level === 0,
    startMs: 5_000,
    maxMs: 60_000,
  },
});
