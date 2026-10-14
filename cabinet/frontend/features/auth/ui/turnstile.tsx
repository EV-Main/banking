"use client";

import { useEffect, useRef } from "react";

import { config } from "@/config";

// Cloudflare Turnstile, explicitly rendered. The identity plane checks the token on its own
// side and refuses a credential request or a code mailing without one, so a widget that fails
// to load leaves the form disabled rather than sending it bare.

interface TurnstileApi {
  render(el: HTMLElement, options: Record<string, unknown>): string;
  reset(id: string): void;
  remove(id: string): void;
}

declare global {
  interface Window {
    turnstile?: TurnstileApi;
  }
}

const SCRIPT = "https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit";
let loading: Promise<TurnstileApi> | null = null;

function loadTurnstile(): Promise<TurnstileApi> {
  loading ??= new Promise((resolve, reject) => {
    if (window.turnstile) return resolve(window.turnstile);
    const script = document.createElement("script");
    script.src = SCRIPT;
    script.async = true;
    script.onload = () => (window.turnstile ? resolve(window.turnstile) : reject(new Error("turnstile did not load")));
    script.onerror = () => {
      loading = null;
      reject(new Error("turnstile did not load"));
    };
    document.head.appendChild(script);
  });
  return loading;
}

/**
 * A token is single-use: whoever spends one bumps `generation`, and the widget re-arms.
 * `onToken(null)` means there is none to send — expired, failed, or not yet solved.
 */
export function Turnstile({ onToken, generation, onUnavailable }: { onToken: (token: string | null) => void; generation: number; onUnavailable: () => void }) {
  const host = useRef<HTMLDivElement>(null);
  const widget = useRef<string | null>(null);
  const callbacks = useRef({ onToken, onUnavailable });
  useEffect(() => {
    callbacks.current = { onToken, onUnavailable };
  });

  useEffect(() => {
    let cancelled = false;
    let api: TurnstileApi | null = null;
    const sitekey = config.public.turnstileSiteKey;
    if (!sitekey) {
      callbacks.current.onUnavailable();
      return;
    }
    loadTurnstile().then(
      (loaded) => {
        if (cancelled || !host.current) return;
        api = loaded;
        widget.current = loaded.render(host.current, {
          sitekey,
          callback: (token: string) => callbacks.current.onToken(token),
          "expired-callback": () => callbacks.current.onToken(null),
          "error-callback": () => callbacks.current.onToken(null),
          theme: "dark",
          size: "flexible",
          appearance: "always",
        });
      },
      () => !cancelled && callbacks.current.onUnavailable(),
    );
    return () => {
      cancelled = true;
      if (api && widget.current) api.remove(widget.current);
      widget.current = null;
    };
  }, []);

  useEffect(() => {
    if (generation === 0 || !widget.current) return;
    callbacks.current.onToken(null);
    window.turnstile?.reset(widget.current);
  }, [generation]);

  return <div ref={host} className="mx-0.5 mt-3 min-h-16.25 [&_iframe]:block" />;
}
