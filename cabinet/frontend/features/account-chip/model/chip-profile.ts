"use client";

import { fetchProfile } from "@/entities/user/api/profile-client";
import { TAG } from "@/shared/lib/cache-tags";
import { defineResource } from "@/shared/lib/resource";

// The chip's own read of the profile, for its name only.
//
// Not `profileResource`: that one polls while `kyc_level` is 0 so the cabinet notices a
// verdict, and the chip — a separate bundle with its own registry — would run that loop a
// second time on every page it sits on, the public site included, to render a label that
// a verdict does not change. A name edit reaches the chip through the identity cache
// (`rememberProfile` in `saveProfile`), so a plain once-per-load read is all it needs.
export const chipProfileResource = defineResource({
  name: "account-chip.profile",
  fetch: fetchProfile,
  revalidate: 60,
  tags: [TAG.profile],
});
