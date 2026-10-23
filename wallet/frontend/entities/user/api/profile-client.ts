// Browser → BFF profile client. Thin typed fetchers over /api/users; the shapes are the
// proto-derived types. Transport, CSRF and session handling belong to
// `@/shared/lib/api-client`. No tokens here — the BFF holds them.

import { getJson, requestJson } from "@/shared/lib/api-client";
import type { UpdateProfileRequest, UserProfile } from "@/shared/contracts";

export function fetchProfile(): Promise<UserProfile> {
  return getJson<UserProfile>("/api/users");
}

export function saveProfile(fields: UpdateProfileRequest): Promise<UserProfile> {
  return requestJson<UserProfile>("/api/users", { method: "PATCH", body: fields });
}
