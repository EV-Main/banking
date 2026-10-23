// A mandate as the BFF serves it (`wallet/backend/src/dto.rs` `Mandate`). Hand-written, not
// the generated type: the BFF lowercases the operation names and renders every i64 as a
// string of unix seconds, with "0" meaning none.

export type MandateOperation = "reserve" | "charge" | "book_fill";

export interface Mandate {
  mandate_id: string;
  scope: string;
  client_id: string;
  client_display_name: string;
  cap_per_op: string;
  cap_24h: string;
  cap_outstanding: string;
  cap_book_fill_24h: string;
  operations: MandateOperation[];
  /** Unix seconds; "0" = until revoked. */
  expires_at: string;
  created_at: string;
  /** "0" while live. */
  revoked_at: string;
  /** "0" unless a newer grant replaced it. */
  replaced_at: string;
  used_24h: string;
  outstanding: string;
  book_filled_24h: string;
}

export interface MandateList {
  mandates: Mandate[];
}

/**
 * What a service asked the reader to grant, as the BFF serves it (`GET
 * /api/wallet/mandate-requests/{id}`; aligned with the generated `BankingV1MandateRequest`,
 * hand-written for the same reason as {@link Mandate}: lowercase operations, string stamps).
 * The grant page shows exactly these terms and nothing the URL says.
 */
export interface MandateRequest {
  request_id: string;
  scope: string;
  client_id: string;
  client_display_name: string;
  cap_per_op: string;
  cap_24h: string;
  cap_outstanding: string;
  /** "" unless `operations` lists `book_fill`. */
  cap_book_fill_24h: string;
  operations: MandateOperation[];
  /** Unix seconds of the mandate's own end; "0" = until revoked. */
  mandate_expires_at: string;
  /** Unix seconds until which this request can still be granted. */
  expires_at: string;
}

/** The only body `POST /api/wallet/mandates` accepts: terms are looked up from the request. */
export interface GrantMandateBody {
  request_id: string;
  idempotency_key: string;
}
