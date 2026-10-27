import type { Translate } from "@evinvest/i18n";

import type { IntentError } from "../lib/intent";

/** Every intent error in the reader's words. Exhaustive: a new code is a type error here. */
export function intentErrorText(error: IntentError, t: Translate): string {
  switch (error.code) {
    case "csrf":
      return t("err.csrf", "This page went stale. Reload it and try again.");
    case "unauthenticated":
    case "session_changed":
      return t("err.unauthenticated", "We couldn't confirm your session. Reload the page or sign in again.");
    case "signed_out":
      return t("err.sessionExpired", "Your session has ended. Sign in again to continue.");
    case "rate_limited":
      return t("err.rateLimited", "Too many requests — give it a moment and try again.");
    case "network":
      return t("err.network", "Can't reach the server. Check your connection and try again.");
    case "unavailable":
    case "malformed":
    case "store_unavailable":
    case "identity_unavailable":
    case "unsupported_media_type":
      return t("err.serverUnavailable", "The service is temporarily unavailable. Please try again.");
    case "invalid_request":
    case "proof_not_expected":
      return t("err.requestFailed", "Something went wrong on our side. Please try again.");
    case "invalid_input":
      if (error.field === "amount") return t("walletIntent.err.amount", "Enter an amount greater than zero.");
      if (error.field === "address") return t("walletIntent.err.address", "Check the address. It does not match this network.");
      if (error.field === "network") return t("walletIntent.err.network", "Choose a network.");
      return t("err.requestFailed", "Something went wrong on our side. Please try again.");
    case "not_enabled":
    case "kind_not_enabled":
      return t("walletIntent.err.notEnabled", "This action is not available on this page yet.");
    case "account_not_active":
      return t("walletIntent.err.accountNotActive", "Your account cannot move money right now. Contact support.");
    case "scope_mismatch":
      return t("walletIntent.err.scopeMismatch", "This action does not belong to this service.");
    case "network_unavailable":
      return t("walletIntent.err.networkUnavailable", "Withdrawals on this network are paused right now.");
    case "below_minimum":
      return t("walletIntent.err.belowMinimum", "The amount is below the minimum withdrawal for this network.");
    case "below_fee":
      return t("walletIntent.err.belowFee", "The amount does not cover the network fee.");
    case "invoice_not_open":
      return t("walletAction.invoiceClosed", "This invoice can no longer be paid. Nothing has been charged.");
    case "mandate_request_expired":
      return t("walletAction.requestUnavailable", "This request is not available. It may have expired — ask the service to send a new one.");
    case "intent_not_found":
    case "intent_expired":
    case "terms_changed":
      return t("walletIntent.err.expired", "These terms are no longer valid. Review them again.");
    case "intent_not_pending":
      return t("walletIntent.err.notPending", "This confirmation was already used. Check your recent activity before you try again.");
    case "proof_required":
      return t("walletIntent.stepupTitle", "One more check is needed");
    case "stepup_invalid":
      return t("walletIntent.err.stepupInvalid", "The check did not match this confirmation. Start again.");
    case "proof_replayed":
      return t("walletIntent.err.proofReplayed", "That check was already used. Start again.");
    case "limit_reached":
      return t("walletIntent.err.limit", "You have reached the 24-hour limit for moves without an extra check.");
    case "intent_executing":
      return t("walletIntent.unclear", "Still processing. Nothing is lost — check again in a moment.");
  }
}
