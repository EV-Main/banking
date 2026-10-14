"use client";

import { useT } from "@evinvest/i18n/react";

import { type FormEvent, useEffect, useState } from "react";

import { Button, Input, Skeleton } from "@evinvest/uikit";

import { type AuthError, type SignInMethods, addPasskey, confirmVerification, readMethods, removePasskey, requestVerification, setPassword, setUsername } from "@/features/auth/api/auth-client";
import { passkeysSupported } from "@/features/auth/lib/webauthn";
import { Turnstile } from "@/features/auth/ui/turnstile";
import { refreshSession } from "@/shared/lib/session";
import { Hairline, Pill, Row, RowLabel } from "@/shared/ui/list-card";

type T = ReturnType<typeof useT>;

function refusal(t: T, error: AuthError): string {
  switch (error) {
    case "invalid_code":
      return t("auth.err.invalidCode", "That code is not right. Try again.");
    case "attempts_exceeded":
      return t("auth.err.attemptsExceeded", "Too many tries. Send yourself a new code.");
    case "code_expired":
      return t("auth.err.codeExpired", "That code has expired. Send yourself a new code.");
    case "throttled":
      return t("auth.err.throttled", "Too many codes went to this address. Try again in a few minutes.");
    case "email_taken":
      return t("auth.err.emailVerifiedElsewhere", "This address is verified on another account.");
    case "weak_password":
      return t("auth.err.weakPassword", "Use at least 8 characters.");
    case "invalid_username":
      return t("auth.err.invalidUsername", "3–32 characters: letters, digits, '_', '.' or '-'.");
    case "username_taken":
      return t("auth.err.usernameTaken", "That username is taken.");
    case "passkey_cancelled":
      return t("auth.err.passkeyCancelled", "Passkey sign in was cancelled.");
    case "passkey_registered":
      return t("auth.err.passkeyRegistered", "This passkey is registered already.");
    case "captcha":
      return t("auth.err.captcha", "Captcha verification failed. Please try again.");
    case "captcha_unavailable":
      return t("auth.err.captchaUnavailable", "We could not check the captcha just now. Please try again.");
    default:
      return t("auth.err.generic", "Sign-in failed. Please try again.");
  }
}

/**
 * Asking for a code spends the account's daily budget, not one code's: `refusal` would tell
 * the reader to send a new code, which is exactly what the budget now refuses.
 */
function sendRefusal(t: T, error: AuthError): string {
  switch (error) {
    case "attempts_exceeded":
      return t("auth.err.verifyAttemptsExceeded", "Too many wrong codes in the last 24 hours. Try again tomorrow.");
    case "throttled":
      return t("auth.err.verifyThrottled", "Too many codes went to this address. Try again later — the limit is 10 a day.");
    default:
      return refusal(t, error);
  }
}

type Editing = null | "verify" | "password" | "username";

/**
 * How the signed-in account signs in: its address and whether it is proven, its username,
 * its providers and password — and the three things a reader changes here. Proving the
 * address and setting a password both spend a mailed code: a password a stolen session
 * could plant without one would outlive that session's revocation.
 */
export function SignInMethodRows() {
  const t = useT();
  const [methods, setMethods] = useState<SignInMethods | null | undefined>(undefined);
  const [editing, setEditing] = useState<Editing>(null);

  useEffect(() => {
    void readMethods().then(setMethods);
  }, []);

  async function done() {
    setEditing(null);
    setMethods(await readMethods());
    // The session carries `emailVerified`, which the KYC row reads.
    await refreshSession();
  }

  if (methods === undefined) return <Skeleton className="my-3.5 h-10 w-full" />;
  if (methods === null) return <p className="py-3.5 text-xs text-ink-soft">—</p>;

  const providers = methods.providers.map((p) => (p === "google" ? "Google" : p === "github" ? "GitHub" : p));
  const ways = [...providers, ...(methods.password ? [t("auth.method.password", "Password")] : []), t("auth.method.code", "Email code")];
  return (
    <>
      <Row>
        <RowLabel title={methods.email} sub={methods.emailVerified ? t("auth.email.verified", "Verified") : t("auth.email.unverifiedSub", "Not verified — identity checks need a verified email")} />
        {methods.emailVerified ? (
          <Pill>{t("auth.email.verified", "Verified")}</Pill>
        ) : (
          <Button variant="outline" size="sm" onClick={() => setEditing("verify")}>
            {t("auth.email.verify", "Verify")}
          </Button>
        )}
      </Row>
      {editing === "verify" && <CodeForm t={t} action={t("auth.email.confirm", "Confirm")} submit={(code) => confirmVerification(code)} onDone={done} />}
      <Hairline />
      <Row>
        <RowLabel title={t("auth.username.title", "Username")} sub={methods.username ? `@${methods.username}` : t("auth.username.none", "None yet — once set, it signs you in like your email")} />
        <Button variant="outline" size="sm" onClick={() => setEditing(editing === "username" ? null : "username")}>
          {t("auth.username.change", "Change")}
        </Button>
      </Row>
      {editing === "username" && <UsernameForm t={t} current={methods.username ?? ""} onDone={done} />}
      <Hairline />
      <Row>
        <RowLabel title={t("auth.methods.title", "Sign-in methods")} sub={ways.join(" · ")} />
        <Button variant="outline" size="sm" onClick={() => setEditing(editing === "password" ? null : "password")}>
          {methods.password ? t("auth.password.change", "Change password") : t("auth.password.set", "Set password")}
        </Button>
      </Row>
      {editing === "password" && <CodeForm t={t} withPassword action={t("auth.password.save", "Save password")} submit={(code, password) => setPassword(password ?? "", code)} onDone={done} />}
      <Hairline />
      <Passkeys t={t} methods={methods} onChanged={done} />
    </>
  );
}

/** Mail a code to the account's address once the reader passes the challenge, then spend it on `submit`. */
function CodeForm({
  t,
  action,
  withPassword = false,
  submit,
  onDone,
}: {
  t: T;
  action: string;
  withPassword?: boolean;
  submit: (code: string, password?: string) => Promise<{ ok: true } | { ok: false; error: AuthError }>;
  onDone: () => void;
}) {
  const [sent, setSent] = useState(false);
  const [code, setCode] = useState("");
  const [password, setPasswordValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [token, setToken] = useState<string | null>(null);
  const [tokenGeneration, setTokenGeneration] = useState(0);
  const [captchaBroken, setCaptchaBroken] = useState(false);

  async function send() {
    if (token === null) return;
    setBusy(true);
    setError(null);
    const answer = await requestVerification(token);
    // Single-use, and whether the plane spent it before refusing is its business: re-arm on
    // every answer so a retry never replays a token Cloudflare has already seen.
    setToken(null);
    setTokenGeneration((g) => g + 1);
    setBusy(false);
    if (answer.ok) setSent(true);
    else setError(sendRefusal(t, answer.error));
  }

  async function confirm(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    const answer = await submit(code.trim(), withPassword ? password : undefined);
    setBusy(false);
    if (answer.ok) onDone();
    else setError(refusal(t, answer.error));
  }

  return (
    <div className="flex flex-col gap-2 pb-3.5">
      {!sent ? (
        <>
          <Turnstile
            generation={tokenGeneration}
            onToken={setToken}
            onUnavailable={() => {
              setCaptchaBroken(true);
              setError(t("auth.err.captchaLoad", "Captcha failed to load. Please refresh the page."));
            }}
          />
          <Button size="sm" className="self-start" disabled={busy || token === null || captchaBroken} onClick={send}>
            {t("auth.code.mailMe", "Email me a code")}
          </Button>
        </>
      ) : (
        <form className="flex flex-col gap-2" onSubmit={confirm}>
          <Input inputMode="numeric" autoComplete="one-time-code" placeholder={t("auth.code.label", "Code")} value={code} onChange={(e) => setCode(e.target.value)} />
          {withPassword && (
            <Input type="password" autoComplete="new-password" placeholder={t("auth.field.newPassword", "New password")} value={password} onChange={(e) => setPasswordValue(e.target.value)} />
          )}
          <Button type="submit" size="sm" className="self-start" disabled={busy || code.trim().length !== 6 || (withPassword && password.length < 8)}>
            {action}
          </Button>
        </form>
      )}
      {error && <p className="text-xs text-accent-error">{error}</p>}
    </div>
  );
}

function UsernameForm({ t, current, onDone }: { t: T; current: string; onDone: () => void }) {
  const [value, setValue] = useState(current.includes("@") ? "" : current);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function save(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    const answer = await setUsername(value.trim());
    setBusy(false);
    if (answer.ok) onDone();
    else setError(refusal(t, answer.error));
  }

  return (
    <form className="flex flex-col gap-2 pb-3.5" onSubmit={save}>
      <Input autoComplete="username" value={value} onChange={(e) => setValue(e.target.value)} />
      <Button type="submit" size="sm" className="self-start" disabled={busy || value.trim().length < 3}>
        {t("auth.username.save", "Save")}
      </Button>
      {error && <p className="text-xs text-accent-error">{error}</p>}
    </form>
  );
}

function Passkeys({ t, methods, onChanged }: { t: T; methods: SignInMethods; onChanged: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function act(action: () => Promise<{ ok: true } | { ok: false; error: AuthError }>) {
    setBusy(true);
    setError(null);
    const answer = await action();
    setBusy(false);
    if (answer.ok) onChanged();
    else setError(refusal(t, answer.error));
  }

  return (
    <>
      <Row>
        <RowLabel title={t("auth.passkeys.title", "Passkeys")} sub={t("auth.passkeys.sub", "Sign in with your fingerprint, face or screen lock — no code, no password")} />
        {passkeysSupported() && (
          <Button variant="outline" size="sm" disabled={busy} onClick={() => act(() => addPasskey(`${t("auth.passkeys.default", "Passkey")} · ${new Date().toLocaleDateString()}`))}>
            {t("auth.passkeys.add", "Add a passkey")}
          </Button>
        )}
      </Row>
      {methods.passkeys.map((passkey) => (
        <Row key={passkey.id} className="pt-0">
          <RowLabel title={passkey.name} />
          <Button variant="outline" size="sm" disabled={busy} onClick={() => act(() => removePasskey(passkey.id))}>
            {t("auth.passkeys.remove", "Remove")}
          </Button>
        </Row>
      ))}
      {error && <p className="pb-3.5 text-xs text-accent-error">{error}</p>}
    </>
  );
}
