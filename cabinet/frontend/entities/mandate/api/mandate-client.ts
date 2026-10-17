// Browser → BFF client for the caller's mandates. A grant names a mandate request the server
// holds (`fetchMandateRequest`) and nothing else: service and caps never travel from here.

import type { GrantMandateBody, Mandate, MandateList, MandateRequest } from "@/shared/contracts";
import { getJson, postJson } from "@/shared/lib/api-client";

export function fetchMandates(): Promise<MandateList> {
  return getJson<MandateList>("/api/wallet/mandates");
}

export function revokeMandate(id: string): Promise<Mandate> {
  return postJson<Mandate>(`/api/wallet/mandates/${encodeURIComponent(id)}/revoke`, {});
}

/** 404 for an unknown, expired or someone else's request alike — the page does not tell them apart. */
export function fetchMandateRequest(id: string): Promise<MandateRequest> {
  return getJson<MandateRequest>(`/api/wallet/mandate-requests/${encodeURIComponent(id)}`);
}

export function grantMandate(body: GrantMandateBody): Promise<Mandate> {
  return postJson<Mandate>("/api/wallet/mandates", body);
}
