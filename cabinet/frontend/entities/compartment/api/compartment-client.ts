// Browser → BFF client for the caller's compartments. Which two ends a move joins and how
// much is the page's input; whether the caller may is the hub's verdict, not ours.

import type { CompartmentList, CompartmentMove } from "@/shared/contracts";
import { getJson, postJson } from "@/shared/lib/api-client";

export function fetchCompartments(): Promise<CompartmentList> {
  return getJson<CompartmentList>("/api/wallet/compartments");
}

export function moveFunds(body: { from_scope: string; to_scope: string; amount: string; idempotency_key: string }): Promise<CompartmentMove> {
  return postJson<CompartmentMove>("/api/wallet/compartments/move", body);
}
