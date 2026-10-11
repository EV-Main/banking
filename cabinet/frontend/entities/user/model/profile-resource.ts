"use client";

// The caller's profile: one read shared by the sidebar account chip, the Profile page and
// Settings, and the one mutation that moves it.
//
// `saveProfile` publishes its own response rather than invalidating: a PATCH answers with
// the profile it just wrote, so a refetch would ask the BFF a question it has already been
// told the answer to. That write-through is what makes the chip's name change the instant
// the form saves — the behaviour the old `publishProfile` seam existed to provide, now the
// default for every consumer.

import { fetchProfile, saveProfile as saveProfileRequest } from "@/entities/user/api/profile-client";
import type { UpdateProfileRequest, UserProfile } from "@/shared/contracts";
import { TAG } from "@/shared/lib/cache-tags";
import { defineResource } from "@/shared/lib/resource";

import { rememberProfile } from "./identity-cache";

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

export async function saveProfile(fields: UpdateProfileRequest): Promise<UserProfile> {
  const updated = await saveProfileRequest(fields);
  profileResource.publish(updated);
  // The header chip is another bundle with its own resource cache; this is how it hears.
  rememberProfile(updated);
  return updated;
}
