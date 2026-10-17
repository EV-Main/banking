// What the payer's page does with an invoice's wire `state`. Pure and import-free so
// `node --test` runs it as is.
//
// `paid` means the ledger has posted the payment. Between the click and the post the server
// answers `processing`; it ends as `paid`, or falls back to `open` when the ledger refuses.

export type InvoicePhase = "payable" | "processing" | "paid" | "closed";

/** A state this page has not met reads as closed: nothing is offered that the server did not. */
export function invoicePhase(state: string | undefined): InvoicePhase {
  switch (state) {
    case "open":
      return "payable";
    case "processing":
      return "processing";
    case "paid":
      return "paid";
    default:
      return "closed";
  }
}

/** How often, and for how long, a page re-reads a processing invoice. */
export const POLL_EVERY_MS = 1500;
export const POLL_GIVE_UP_MS = 60_000;

/** Whether to read again: still processing and within the polling window. */
export function shouldPoll(phase: InvoicePhase, waitedMs: number): boolean {
  return phase === "processing" && waitedMs < POLL_GIVE_UP_MS;
}
