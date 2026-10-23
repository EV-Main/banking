"use client";

import { useT } from "@evinvest/i18n/react";

import { KeyRound } from "lucide-react";
import { type FormEvent, type ReactNode, useEffect, useState, useSyncExternalStore } from "react";

import { Button, Checkbox, Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle, Input, InputOTP, InputOTPGroup, InputOTPSlot, Spinner } from "@evinvest/uikit";

import { type AuthAnswer, type AuthError, PROVIDERS, type Provider, confirmVerification, passkeySignIn, passwordSignIn, passwordSignUp, requestCode, verifyCode } from "@/features/auth/api/auth-client";
import { passkeysSupported } from "@/features/auth/lib/webauthn";
import { loginHref } from "@/features/auth/lib/return-to";
import { useSignInDialog } from "@/features/auth/model/use-sign-in-dialog";
import { GithubMark, GoogleMark } from "@/features/auth/ui/provider-marks";
import { Turnstile } from "@/features/auth/ui/turnstile";
import { useLocale } from "@/shared/lib/cabinet-route";
import { Logo } from "@/shared/ui/logo";

type Screen = { kind: "code" } | { kind: "codeSent"; email: string } | { kind: "password" } | { kind: "signup" } | { kind: "verifyNow"; email: string };

const CODE_LENGTH = 6;
const RESEND_SECS = 60;

// Proportions are openmarket's sign-in form (concierge `tmp/research/openmarket`), on our tokens.
const FIELD =
  "h-12 rounded-(--sign-in-radius) border-ink/10 bg-ink/2 px-3.5 text-(length:--sign-in-text-body) shadow-none transition-[background-color,border-color] hover:border-ink/14 focus-visible:border-ink/18 focus-visible:bg-ink/4 focus-visible:ring-0 md:text-(length:--sign-in-text-body)";
const PRIMARY =
  "h-11.5 w-full rounded-(--sign-in-radius) bg-ink text-sm font-bold tracking-[-0.01em] text-background hover:bg-ink/90 disabled:bg-ink/4 disabled:text-ink-soft disabled:opacity-100";
const SECONDARY =
  "inline-flex h-11.5 w-full cursor-pointer items-center justify-center gap-2.5 rounded-(--sign-in-radius) border border-transparent bg-ink/4 px-3.5 text-(length:--sign-in-text-body) font-semibold tracking-[-0.01em] text-ink transition-colors hover:border-ink/14 hover:bg-ink/6 disabled:cursor-default disabled:hover:border-transparent";

type T = ReturnType<typeof useT>;

function errorText(t: T, error: AuthError | "captcha_load"): string {
  switch (error) {
    case "invalid_code":
      return t("auth.err.invalidCode", "That code is not right. Try again.");
    case "attempts_exceeded":
      return t("auth.err.attemptsExceeded", "Too many tries. Send yourself a new code.");
    case "code_expired":
      return t("auth.err.codeExpired", "That code has expired. Send yourself a new code.");
    case "throttled":
      return t("auth.err.throttled", "Too many codes went to this address. Try again in a few minutes.");
    case "captcha":
      return t("auth.err.captcha", "Captcha verification failed. Please try again.");
    case "captcha_unavailable":
      return t("auth.err.captchaUnavailable", "We could not check the captcha just now. Please try again.");
    case "captcha_load":
      return t("auth.err.captchaLoad", "Captcha failed to load. Please refresh the page.");
    case "invalid_email":
      return t("auth.err.invalidEmail", "Enter a valid email address.");
    case "email_taken":
      return t("auth.err.emailTaken", "An account already exists for that address. Sign in instead — with a code if you forgot the password.");
    case "weak_password":
      return t("auth.err.weakPassword", "Use at least 8 characters.");
    case "invalid_credentials":
      return t("auth.err.invalidCredentials", "Wrong email, username or password.");
    case "password_locked":
      return t("auth.err.passwordLocked", "Too many wrong passwords. Sign in with a code instead.");
    case "disabled":
      return t("auth.err.disabled", "This account is suspended.");
    case "passkey_cancelled":
      return t("auth.err.passkeyCancelled", "Passkey sign in was cancelled.");
    case "passkey_rejected":
      return t("auth.err.passkeyRejected", "That passkey is not registered here. Sign in with a code instead.");
    case "passkey_expired":
      return t("auth.err.passkeyExpired", "That took too long. Try the passkey again.");
    default:
      return t("auth.err.generic", "Sign-in failed. Please try again.");
  }
}

/** What an OAuth round trip that failed puts on the page it returns to (`?auth_error=`). */
function oauthErrorText(t: T, reason: string): string {
  switch (reason) {
    case "denied":
      return t("auth.err.denied", "Sign-in was cancelled.");
    case "invalid":
      return t("auth.err.invalid", "That sign-in attempt expired. Please try again.");
    case "exchange":
      return t("auth.err.exchange", "We couldn't complete sign-in. Please try again.");
    case "disabled":
      return errorText(t, "disabled");
    default:
      return errorText(t, "internal");
  }
}

/**
 * "Sign in or create an account" — one dialog for every method. Opened by `?login` on any
 * cabinet page (`useSignInDialog`), so a link, an AuthWall and the rail open the same one.
 * A success reloads the page it was opened over, now as the signed-in account's.
 */
export function SignInDialog() {
  const t = useT();
  const locale = useLocale();
  const dialog = useSignInDialog();
  const [screen, setScreen] = useState<Screen>({ kind: "code" });
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [token, setToken] = useState<string | null>(null);
  const [tokenGeneration, setTokenGeneration] = useState(0);
  const [captchaBroken, setCaptchaBroken] = useState(false);
  // The dialog renders through a portal the server does not draw: opening it before
  // hydration would make the two disagree.
  const hydrated = useSyncExternalStore(
    () => () => {},
    () => true,
    () => false,
  );

  function go(next: Screen) {
    setError(null);
    setScreen(next);
  }

  /**
   * Send the challenge token with one request. The identity plane refuses `origin` and
   * `captcha_unavailable` before Cloudflare consumed it, so those leave it for the retry;
   * anything else spent it and the widget re-arms.
   */
  async function spending<B>(request: (token: string) => Promise<AuthAnswer<B>>): Promise<AuthAnswer<B>> {
    const answer = await request(token ?? "");
    if (answer.ok || (answer.error !== "captcha_unavailable" && answer.error !== "origin")) {
      setToken(null);
      setTokenGeneration((g) => g + 1);
    }
    return answer;
  }

  async function run(action: () => Promise<void>) {
    setBusy(true);
    setError(null);
    try {
      await action();
    } finally {
      setBusy(false);
    }
  }

  function close(open: boolean) {
    if (open || busy) return;
    go({ kind: "code" });
    dialog.hide();
  }

  const human = token !== null && !captchaBroken;
  const challenge = (
    <Turnstile
      generation={tokenGeneration}
      onToken={setToken}
      onUnavailable={() => {
        setCaptchaBroken(true);
        setError(errorText(t, "captcha_load"));
      }}
    />
  );
  const shown = error ?? (screen.kind === "code" && dialog.oauthError ? oauthErrorText(t, dialog.oauthError) : null);

  return (
    <Dialog open={hydrated && dialog.open} onOpenChange={close}>
      <DialogContent className="gap-6 max-w-[min(25rem,calc(100%-2rem))] rounded-2xl px-7 pt-8 pb-7 sm:max-w-100">
        <Logo className="mx-auto h-7 w-auto text-ink" />
        {screen.kind === "code" && (
          <CodeScreen
            busy={busy}
            human={human}
            challenge={challenge}
            onSubmit={(email) =>
              run(async () => {
                const answer = await spending((tk) => requestCode(email, tk));
                if (answer.ok) go({ kind: "codeSent", email });
                else setError(errorText(t, answer.error));
              })
            }
            onPassword={() => go({ kind: "password" })}
          >
            <Providers
              hrefFor={(provider) => loginHref(locale, dialog.returnTo, provider)}
              onPasskey={() =>
                run(async () => {
                  const answer = await passkeySignIn();
                  if (answer.ok) dialog.reload();
                  else setError(errorText(t, answer.error));
                })
              }
            />
          </CodeScreen>
        )}
        {screen.kind === "codeSent" && (
          <CodeEntry
            title={t("auth.code.title", "Check your email")}
            description={t("auth.code.desc", "We sent a 6 digit sign in code to {email}", { email: screen.email })}
            busy={busy}
            human={human}
            challenge={challenge}
            onCode={(code) =>
              run(async () => {
                const answer = await verifyCode(screen.email, code);
                if (answer.ok) dialog.reload();
                else setError(errorText(t, answer.error));
              })
            }
            onResend={() =>
              run(async () => {
                const answer = await spending((tk) => requestCode(screen.email, tk));
                if (!answer.ok) setError(errorText(t, answer.error));
              })
            }
            onBack={() => go({ kind: "code" })}
          />
        )}
        {screen.kind === "password" && (
          <PasswordScreen
            busy={busy}
            human={human}
            challenge={challenge}
            onSubmit={(identifier, password) =>
              run(async () => {
                const answer = await spending((tk) => passwordSignIn(identifier, password, tk));
                if (answer.ok) dialog.reload();
                else setError(errorText(t, answer.error));
              })
            }
            onCode={() => go({ kind: "code" })}
            onSignUp={() => go({ kind: "signup" })}
          />
        )}
        {screen.kind === "signup" && (
          <SignUpScreen
            busy={busy}
            human={human}
            challenge={challenge}
            onSubmit={(email, password, verify) =>
              run(async () => {
                const answer = await spending((tk) => passwordSignUp(email, password, verify, tk));
                if (!answer.ok) return setError(errorText(t, answer.error));
                if (answer.body.verification === "sent") go({ kind: "verifyNow", email });
                else dialog.reload();
              })
            }
            onSignIn={() => go({ kind: "password" })}
          />
        )}
        {screen.kind === "verifyNow" && (
          <CodeEntry
            title={t("auth.verify.title", "Confirm your email")}
            description={t("auth.verify.desc", "Your account is ready. Enter the 6 digit code we sent to {email} to verify it, or skip and verify later in Settings.", { email: screen.email })}
            busy={busy}
            human
            onCode={(code) =>
              run(async () => {
                const answer = await confirmVerification(code);
                if (answer.ok) dialog.reload();
                else setError(errorText(t, answer.error));
              })
            }
            onBack={() => dialog.reload()}
            backLabel={t("auth.verify.later", "Verify later")}
          />
        )}
        {shown && (
          <p role="status" className="-mt-3 text-center text-xs leading-snug text-accent-error">
            {shown}
          </p>
        )}
      </DialogContent>
    </Dialog>
  );
}

function Heading({ title, description }: { title: string; description?: string }) {
  return (
    <DialogHeader className="gap-1.5 text-center sm:text-center">
      <DialogTitle className="text-(length:--sign-in-text-title) leading-normal tracking-[-0.01em]">{title}</DialogTitle>
      {description && <DialogDescription className="text-(length:--sign-in-text-body) leading-snug">{description}</DialogDescription>}
    </DialogHeader>
  );
}

function Switch({ onClick, children }: { onClick: () => void; children: ReactNode }) {
  return (
    <button type="button" onClick={onClick} className="cursor-pointer text-xs font-medium text-ink-soft transition-colors hover:text-ink">
      {children}
    </button>
  );
}

function looksLikeEmail(raw: string): boolean {
  return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(raw.trim());
}

function submitWith(handler: () => void) {
  return (event: FormEvent) => {
    event.preventDefault();
    handler();
  };
}

function CodeScreen({
  busy,
  human,
  challenge,
  onSubmit,
  onPassword,
  children,
}: {
  busy: boolean;
  human: boolean;
  challenge: ReactNode;
  onSubmit: (email: string) => void;
  onPassword: () => void;
  children: ReactNode;
}) {
  const t = useT();
  const [email, setEmail] = useState("");
  return (
    <>
      <Heading title={t("auth.dialog.heading", "Sign in or create an account")} />
      <form className="flex flex-col gap-6" onSubmit={submitWith(() => onSubmit(email.trim()))}>
        <div className="flex flex-col">
          <Input
            type="email"
            autoComplete="email"
            placeholder={t("auth.field.email", "Email")}
            aria-label={t("auth.field.email", "Email")}
            className={FIELD}
            value={email}
            onChange={(e) => setEmail(e.target.value)}
          />
          {challenge}
        </div>
        <Button type="submit" className={PRIMARY} disabled={busy || !human || !looksLikeEmail(email)}>
          {busy ? <Spinner /> : t("auth.useLoginCode", "Email me a code")}
        </Button>
      </form>
      <div className="-mt-3 flex justify-center">
        <Switch onClick={onPassword}>{t("auth.usePasswordInstead", "Sign in with a password instead")}</Switch>
      </div>
      {children}
    </>
  );
}

function Providers({ hrefFor, onPasskey }: { hrefFor: (provider: Provider) => string; onPasskey: () => void }) {
  const t = useT();
  const label: Record<Provider, string> = { google: "Google", github: "GitHub" };
  const mark: Record<Provider, ReactNode> = { google: <GoogleMark />, github: <GithubMark /> };
  return (
    <>
      <div className="flex items-center gap-3.5 text-(length:--sign-in-text-fine) font-medium text-ink-soft">
        <span className="h-px flex-1 bg-ink/6" />
        {t("auth.orContinueWith", "Or continue with")}
        <span className="h-px flex-1 bg-ink/6" />
      </div>
      <div className="flex flex-col gap-2">
        {passkeysSupported() && (
          <button type="button" className={SECONDARY} onClick={onPasskey}>
            <KeyRound className="size-4" aria-hidden />
            {t("auth.usePasskey", "Use a passkey")}
          </button>
        )}
        {PROVIDERS.map((provider) => (
          // A full navigation: the provider's consent screen is another origin.
          <a key={provider} href={hrefFor(provider)} className={SECONDARY}>
            {mark[provider]}
            {label[provider]}
          </a>
        ))}
      </div>
    </>
  );
}

function CodeEntry({
  title,
  description,
  busy,
  human,
  challenge,
  onCode,
  onResend,
  onBack,
  backLabel,
}: {
  title: string;
  description: string;
  busy: boolean;
  human: boolean;
  challenge?: ReactNode;
  onCode: (code: string) => void;
  onResend?: () => void;
  onBack: () => void;
  backLabel?: string;
}) {
  const t = useT();
  const [code, setCode] = useState("");
  const [wait, setWait] = useState(RESEND_SECS);

  useEffect(() => {
    if (wait <= 0) return;
    const timer = setTimeout(() => setWait((s) => s - 1), 1000);
    return () => clearTimeout(timer);
  }, [wait]);

  function change(raw: string) {
    const digits = raw.replace(/\D/g, "").slice(0, CODE_LENGTH);
    setCode(digits);
    if (digits.length === CODE_LENGTH && !busy) onCode(digits);
  }

  return (
    <>
      <Heading title={title} description={description} />
      <div className="flex flex-col items-center gap-3">
        <InputOTP maxLength={CODE_LENGTH} value={code} onChange={change} disabled={busy} aria-label={t("auth.code.label", "Code")} autoFocus>
          <InputOTPGroup>
            {Array.from({ length: CODE_LENGTH }, (_, i) => (
              <InputOTPSlot key={i} index={i} className="h-12 w-11 text-base" />
            ))}
          </InputOTPGroup>
        </InputOTP>
        <p className="text-center text-(length:--sign-in-text-fine) text-ink-soft">{t("auth.code.checkSpam", "It can take a minute. Check your spam folder too.")}</p>
      </div>
      {onResend && wait <= 0 && challenge}
      <div className="flex items-center justify-between gap-3">
        <Switch onClick={onBack}>{backLabel ?? t("auth.code.back", "Use a different email")}</Switch>
        {onResend &&
          (wait > 0 ? (
            <span className="text-xs font-medium text-ink-soft">{t("auth.code.resendIn", "Send a new code in {seconds}s", { seconds: wait })}</span>
          ) : (
            <Switch
              onClick={() => {
                if (!human || busy) return;
                setCode("");
                setWait(RESEND_SECS);
                onResend();
              }}
            >
              {t("auth.code.resend", "Send a new code")}
            </Switch>
          ))}
      </div>
    </>
  );
}

function PasswordScreen({
  busy,
  human,
  challenge,
  onSubmit,
  onCode,
  onSignUp,
}: {
  busy: boolean;
  human: boolean;
  challenge: ReactNode;
  onSubmit: (identifier: string, password: string) => void;
  onCode: () => void;
  onSignUp: () => void;
}) {
  const t = useT();
  const [identifier, setIdentifier] = useState("");
  const [password, setPassword] = useState("");
  return (
    <>
      <Heading title={t("auth.dialog.heading", "Sign in or create an account")} />
      <form className="flex flex-col gap-6" onSubmit={submitWith(() => onSubmit(identifier.trim(), password))}>
        <div className="flex flex-col gap-5">
          <Input
            autoComplete="username"
            placeholder={t("auth.field.identifier", "Email or username")}
            aria-label={t("auth.field.identifier", "Email or username")}
            className={FIELD}
            value={identifier}
            onChange={(e) => setIdentifier(e.target.value)}
          />
          <div className="flex flex-col">
            <Input
              type="password"
              autoComplete="current-password"
              placeholder={t("auth.field.password", "Password")}
              aria-label={t("auth.field.password", "Password")}
              className={FIELD}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
            {challenge}
          </div>
        </div>
        <Button type="submit" className={PRIMARY} disabled={busy || !human || !identifier.trim() || !password}>
          {busy ? <Spinner /> : t("auth.signIn", "Sign in")}
        </Button>
      </form>
      <div className="-mt-3 flex items-center justify-between gap-3">
        <Switch onClick={onCode}>{t("auth.useCodeInstead", "Sign in with a code instead")}</Switch>
        <Switch onClick={onSignUp}>{t("auth.createAccount", "Create an account")}</Switch>
      </div>
    </>
  );
}

function SignUpScreen({
  busy,
  human,
  challenge,
  onSubmit,
  onSignIn,
}: {
  busy: boolean;
  human: boolean;
  challenge: ReactNode;
  onSubmit: (email: string, password: string, verify: boolean) => void;
  onSignIn: () => void;
}) {
  const t = useT();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [verify, setVerify] = useState(true);
  return (
    <>
      <Heading title={t("auth.signup.heading", "Create an account")} description={t("auth.signup.desc", "Your account works right away. A KYC check later needs a verified email.")} />
      <form className="flex flex-col gap-6" onSubmit={submitWith(() => onSubmit(email.trim(), password, verify))}>
        <div className="flex flex-col gap-5">
          <Input
            type="email"
            autoComplete="email"
            placeholder={t("auth.field.email", "Email")}
            aria-label={t("auth.field.email", "Email")}
            className={FIELD}
            value={email}
            onChange={(e) => setEmail(e.target.value)}
          />
          <Input
            type="password"
            autoComplete="new-password"
            placeholder={t("auth.field.password", "Password")}
            aria-label={t("auth.field.password", "Password")}
            className={FIELD}
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
          <div className="flex flex-col">
            <label className="flex items-center gap-2 text-(length:--sign-in-text-body) text-ink-soft">
              <Checkbox checked={verify} onCheckedChange={setVerify} />
              {t("auth.signup.verifyNow", "Verify my email now")}
            </label>
            {challenge}
          </div>
        </div>
        <Button type="submit" className={PRIMARY} disabled={busy || !human || !looksLikeEmail(email) || password.length < 8}>
          {busy ? <Spinner /> : t("auth.signup.create", "Create account")}
        </Button>
      </form>
      <div className="-mt-3 flex justify-center">
        <Switch onClick={onSignIn}>{t("auth.signup.haveAccount", "Already have an account? Sign in")}</Switch>
      </div>
    </>
  );
}
