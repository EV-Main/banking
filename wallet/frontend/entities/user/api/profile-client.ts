// Browser → BFF profile client. Thin typed fetchers over /api/users; the shapes are the
// proto-derived types. Transport, CSRF and session handling belong to
// `@/shared/lib/api-client`. No tokens here — the BFF holds them.

import { getJson } from "@/shared/lib/api-client";
import type { UserProfile } from "@/shared/contracts";

export function fetchProfile(): Promise<UserProfile> {
  return getJson<UserProfile>("/api/users");
}
