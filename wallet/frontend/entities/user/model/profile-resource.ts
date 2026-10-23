"use client";

// The caller's profile: one read shared by the KYC status chip and the deposit and
// withdraw views.

import { fetchProfile } from "@/entities/user/api/profile-client";
import { TAG } from "@/shared/lib/cache-tags";
import { defineResource } from "@/shared/lib/resource";

export const profileResource = defineResource({
  name: "user.profile",
  fetch: fetchProfile,
  revalidate: 60,
  tags: [TAG.profile],
  // A tier-0 caller may have a verification case open at the provider right now, and the
  // hub's own verdict can land while this tab sits open and focused — none of `resource.
  // ts`'s other triggers (mount, focus regained, route warmed) fire for that.
  //
  // What this loop is NOT, any more: the place that notices a verdict. That is
  // `features/kyc/model/kyc-status-resource`, which watches the case itself rather than
  // inferring one from a tier (#190, #219). What is left here is the narrower job the tier
  // field alone can do — this value is MIRRORED from the identity plane across the bridge,
  // so it lands a poll after the plane's own answer, and something has to keep asking until
  // it does. `kyc_level` moving off 0 is that arrival, and closes the loop.
  poll: { while: (p) => (p?.kyc_level ?? 0) === 0, startMs: 5_000, maxMs: 60_000 },
});
