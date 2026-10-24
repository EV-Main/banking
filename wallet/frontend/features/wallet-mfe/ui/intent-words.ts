import type { Translate } from "@evinvest/i18n";

import type { IntentError, StepupMethod } from "../lib/intent";
import type { StepupError } from "../lib/stepup";

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
      if (error.field === "amount") return t("walletMfe.err.amount", "Enter an amount greater than zero.");
      if (error.field === "address") return t("walletMfe.err.address", "Check the address. It does not match this network.");
      if (error.field === "network") return t("walletMfe.err.network", "Choose a network.");
      return t("err.requestFailed", "Something went wrong on our side. Please try again.");
    case "not_enabled":
    case "kind_not_enabled":
      return t("walletMfe.err.notEnabled", "This action is not available on this page yet.");
    case "account_not_active":
      return t("walletMfe.err.accountNotActive", "Your account cannot move money right now. Contact support.");
    case "scope_mismatch":
      return t("walletMfe.err.scopeMismatch", "This action does not belong to this service.");
    case "network_unavailable":
      return t("walletMfe.err.networkUnavailable", "Withdrawals on this network are paused right now.");
    case "below_minimum":
      return t("walletMfe.err.belowMinimum", "The amount is below the minimum withdrawal for this network.");
    case "below_fee":
      return t("walletMfe.err.belowFee", "The amount does not cover the network fee.");
    case "invoice_not_open":
      return t("walletAction.invoiceClosed", "This invoice can no longer be paid. Nothing has been charged.");
    case "mandate_request_expired":
      return t("walletAction.requestUnavailable", "This request is not available. It may have expired — ask the service to send a new one.");
    case "intent_not_found":
    case "intent_expired":
    case "terms_changed":
      return t("walletMfe.err.expired", "These terms are no longer valid. Review them again.");
    case "intent_not_pending":
      return t("walletMfe.err.notPending", "This confirmation was already used. Check your recent activity before you try again.");
    case "proof_required":
      return t("walletMfe.stepupTitle", "One more check is needed");
    case "stepup_invalid":
      return t("walletMfe.err.stepupInvalid", "The check did not match this confirmation. Start again.");
    case "proof_replayed":
      return t("walletMfe.err.proofReplayed", "That check was already used. Start again.");
    case "email_limit_reached":
      return t("walletMfe.err.emailLimit", "You have reached today's limit for confirming with an emailed code.");
    case "limit_reached":
      return t("walletMfe.err.limit", "You have reached the 24-hour limit for moves without an extra check.");
    case "intent_executing":
      return t("walletMfe.unclear", "Still processing. Nothing is lost — check again in a moment.");
  }
}

/** How the extra check can be done, from the methods the BFF offered for this intent. */
export function stepupMethodsText(methods: readonly StepupMethod[], t: Translate): string {
  const passkey = methods.includes("passkey");
  const email = methods.includes("email_code");
  if (passkey && email) return t("walletMfe.stepupPasskeyOrEmail", "Confirming this needs your passkey or a code we email you.");
  if (email) return t("walletMfe.stepupEmail", "Confirming this needs a code we email you.");
  return t("walletMfe.stepupPasskey", "Confirming this needs your passkey.");
}

/** Every step-up error in the reader's words. Exhaustive: a new code is a type error here. */
export function stepupErrorText(error: StepupError, t: Translate): string {
  switch (error.code) {
    case "no_passkey":
      return t("walletMfe.stepup.noPasskey", "This account has no passkey that can confirm this here. Continue on EV Invest.");
    case "throttled":
      return t("walletMfe.stepup.throttled", "Too many checks in a short time. Wait a few minutes, then start again.");
    case "expired":
      return t("walletMfe.stepup.expired", "The check ran out of time. Start again.");
    case "passkey_rejected":
      return t("walletMfe.stepup.rejected", "Your passkey was not accepted. Try again.");
    case "cancelled":
      return t("walletMfe.stepup.cancelled", "The passkey prompt was closed. Try again when you are ready.");
    case "authenticator_failed":
      return t("walletMfe.stepup.noAuthenticator", "This browser could not ask for your passkey. Continue on EV Invest.");
    case "invalid_code":
      return t("walletMfe.stepup.invalidCode", "That code is not right. Check it and try again.");
    case "attempts_exceeded":
      return t("walletMfe.stepup.attempts", "Too many wrong codes. Start again.");
    case "captcha":
      return t("walletMfe.stepup.captcha", "We could not check that a person is asking. Start again.");
    case "unavailable":
      return t("walletMfe.stepup.unavailable", "This check is not available on this page. Continue on EV Invest.");
    case "signed_out":
      return t("err.sessionExpired", "Your session has ended. Sign in again to continue.");
    case "network":
      return t("err.network", "Can't reach the server. Check your connection and try again.");
    case "malformed":
      return t("err.serverUnavailable", "The service is temporarily unavailable. Please try again.");
  }
}
