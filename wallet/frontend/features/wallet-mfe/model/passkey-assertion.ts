import { assertionJson, passkeysSupported, requestOptions } from "@/shared/lib/webauthn";

import { assertionErrorOf, type PasskeyOptions, type StepupResult } from "../lib/stepup";

/**
 * Asks the authenticator with the server's options as they came: `rpId` is the server's
 * (the registrable domain), never set here. Runs in the host page's own document, not a
 * frame, so no `publickey-credentials-get` delegation is involved; a host that sends a
 * Permissions-Policy denying it to itself turns this off, and the popup remains.
 */
export async function assertPasskey(options: PasskeyOptions): Promise<StepupResult<Record<string, unknown>>> {
  if (!passkeysSupported()) return { ok: false, error: { code: "authenticator_failed" } };
  try {
    const credential = await navigator.credentials.get(requestOptions(options));
    if (!(credential instanceof PublicKeyCredential)) return { ok: false, error: { code: "cancelled" } };
    return { ok: true, value: assertionJson(credential) };
  } catch (error) {
    return { ok: false, error: assertionErrorOf(error instanceof DOMException ? error.name : "") };
  }
}
