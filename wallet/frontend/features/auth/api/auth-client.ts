// The identity plane's sign-in surface, reached at the site root (`/api/auth/*`, which the
// conductor sends to concierge) — not through the wallet host's BFF, which never sees a
// credential. Every answer is `{"ok":true,…}` or `{"error":"<code>"}` from one closed
// vocabulary; the dialog words the code.

import { assertionJson, creationOptions, registrationJson, requestOptions } from "@/features/auth/lib/webauthn";
import { csrfHeader } from "@/shared/lib/csrf-client";

export type AuthError =
  | "origin"
  | "captcha"
  | "captcha_unavailable"
  | "invalid_email"
  | "throttled"
  | "invalid_code"
  | "code_expired"
  | "attempts_exceeded"
  | "email_taken"
  | "weak_password"
  | "invalid_credentials"
  | "password_locked"
  | "disabled"
  | "unauthenticated"
  | "csrf"
  | "invalid_username"
  | "username_taken"
  | "passkey_expired"
  | "passkey_rejected"
  | "passkey_registered"
  | "passkey_cancelled"
  | "internal";

export type AuthAnswer<T = Record<string, unknown>> = { ok: true; body: T } | { ok: false; error: AuthError };

async function post<T>(path: string, body: unknown, signedIn = false): Promise<AuthAnswer<T>> {
  let res: Response;
  try {
    res = await fetch(`/api/auth${path}`, {
      method: "POST",
      headers: { "content-type": "application/json", accept: "application/json", ...(signedIn ? csrfHeader() : {}) },
      body: JSON.stringify(body),
    });
  } catch {
    return { ok: false, error: "internal" };
  }
  const json = (await res.json().catch(() => null)) as (T & { error?: string }) | null;
  if (res.ok && json) return { ok: true, body: json };
  return { ok: false, error: (json?.error as AuthError | undefined) ?? "internal" };
}

export const requestCode = (email: string, turnstileToken: string) => post("/code/request", { email, turnstileToken });

export const verifyCode = (email: string, code: string) => post("/code/verify", { email, code });

export const passwordSignIn = (identifier: string, password: string, turnstileToken: string) =>
  post("/password/signin", { identifier, password, turnstileToken });

export const passwordSignUp = (email: string, password: string, verify: boolean, turnstileToken: string) =>
  post<{ verification: "sent" | "skipped" | "throttled" | "failed" }>("/password/signup", { email, password, verify, turnstileToken });

/** Mail the signed-in account a code for its own address. It sends mail, so like `/code/request` it carries a challenge token: a session alone must not be able to drive the mailer. */
export const requestVerification = (turnstileToken: string) => post("/email/verify/request", { turnstileToken }, true);

export const confirmVerification = (code: string) => post("/email/verify/confirm", { code }, true);

export const setPassword = (password: string, code: string) => post("/password/set", { password, code }, true);

export const setUsername = (username: string) => post<{ username: string }>("/username", { username }, true);

export interface PasskeySummary {
  id: string;
  name: string;
  createdAt: number;
  lastUsedAt: number | null;
}

export interface SignInMethods {
  email: string;
  emailVerified: boolean;
  username: string | null;
  password: boolean;
  providers: string[];
  passkeys: PasskeySummary[];
}

type Ceremony = { ceremony: string; options: { publicKey: Record<string, unknown> } };

/** The browser's prompt, or `null` when the reader dismissed it. */
async function prompt<T>(ask: () => Promise<Credential | null>, done: (credential: PublicKeyCredential) => T): Promise<T | null> {
  try {
    const credential = await ask();
    return credential ? done(credential as PublicKeyCredential) : null;
  } catch {
    return null;
  }
}

/** Register a passkey for the signed-in account under `name`. */
export async function addPasskey(name: string): Promise<AuthAnswer> {
  const begun = await post<Ceremony>("/passkey/register/options", {}, true);
  if (!begun.ok) return begun;
  const credential = await prompt(() => navigator.credentials.create(creationOptions(begun.body.options)), registrationJson);
  if (!credential) return { ok: false, error: "passkey_cancelled" };
  return post("/passkey/register/verify", { ceremony: begun.body.ceremony, credential, name }, true);
}

/** Sign in with whichever passkey the reader picks; it names the account. */
export async function passkeySignIn(): Promise<AuthAnswer> {
  const begun = await post<Ceremony>("/passkey/signin/options", {});
  if (!begun.ok) return begun;
  const credential = await prompt(() => navigator.credentials.get(requestOptions(begun.body.options)), assertionJson);
  if (!credential) return { ok: false, error: "passkey_cancelled" };
  return post("/passkey/signin/verify", { ceremony: begun.body.ceremony, credential });
}

export const removePasskey = (credentialId: string) => post("/passkey/remove", { credentialId }, true);

export async function readMethods(): Promise<SignInMethods | null> {
  const res = await fetch("/api/auth/methods", { headers: { accept: "application/json" }, cache: "no-store" }).catch(() => null);
  return res?.ok ? ((await res.json()) as SignInMethods) : null;
}

/** The OAuth providers the plane signs in through, by their `?provider=` name. */
export const PROVIDERS = ["google", "github"] as const;
export type Provider = (typeof PROVIDERS)[number];
