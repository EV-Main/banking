// Browser → BFF client for the caller's mandates. Revoking needs only the id the server
// listed; granting is not here — see the mandate page for why.

import type { Mandate, MandateList } from "@/shared/contracts";
import { getJson, postJson } from "@/shared/lib/api-client";

export function fetchMandates(): Promise<MandateList> {
  return getJson<MandateList>("/api/wallet/mandates");
}

export function revokeMandate(id: string): Promise<Mandate> {
  return postJson<Mandate>(`/api/wallet/mandates/${encodeURIComponent(id)}/revoke`, {});
}
