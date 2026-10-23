"use client";

// The one composition primitive the cabinet host knows. It loads a microfrontend's
// self-registering ESM bundle by URL, waits for its custom element to upgrade,
// then mounts <tag> inside a host node — mapping props to attributes and
// CustomEvents back to callbacks. Identical for React and Rust/WASM microfrontends.
//
// Remotes are client-only (registration runs in the browser), so this is a
// 'use client' island: server components above stream normally and show the
// fallback until the element is ready. The element is created imperatively (not
// via JSX) so it works for any framework's custom element without React's
// attribute/property quirks.
//
// Trust boundary: a remote runs as first-party JS in the session-bearing origin,
// so its bundle is only injected after its scriptUrl's origin clears the allow-list
// (shared/mfe/validate.ts) — a defensive re-check of what the server loader already
// enforced — and is pinned with Subresource-Integrity + crossOrigin. The registry
// (validate.ts) is the trust anchor; attribute/event mapping is NOT a sanitization
// boundary, so callers must never forward secrets through attributes, and inbound
// CustomEvent detail (typed `unknown`) is untrusted input from the remote — host
// callbacks must validate it before letting it into host state.

import { useEffect, useRef, useState, type ReactNode } from "react";

import { isAllowedScriptUrl } from "./validate";

export interface RemoteElementProps {
  /** Custom-element tag the remote registers, e.g. "mfe-risk-pnl-chart". */
  tag: string;
  /** URL of the remote's self-registering ESM bundle. */
  scriptUrl: string;
  /** Subresource-Integrity hash for the bundle, delivered with `scriptUrl`. */
  integrity?: string;
  /** Attributes passed down to the element (objects are JSON-encoded). */
  attributes?: Record<string, string | number | boolean | object>;
  /** CustomEvent name → handler, e.g. { select: (detail) => ... }. detail is untrusted. */
  events?: Record<string, (detail: unknown) => void>;
  className?: string;
  /** Shown until the element is registered (and if loading fails). */
  fallback?: ReactNode;
}

// Stable empty defaults so the attribute/event effects don't re-run on every render.
const NO_ATTRS: Record<string, string | number | boolean | object> = {};
const NO_EVENTS: Record<string, (detail: unknown) => void> = {};

export function RemoteElement({ tag, scriptUrl, integrity, attributes = NO_ATTRS, events = NO_EVENTS, className, fallback = null }: RemoteElementProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const elementRef = useRef<HTMLElement | null>(null);
  const [ready, setReady] = useState(false);

  // Load the remote's bundle (once per tag), then wait for the element to upgrade.
  useEffect(() => {
    let cancelled = false;
    const whenReady = () => customElements.whenDefined(tag).then(() => !cancelled && setReady(true));

    if (customElements.get(tag) || document.querySelector(`script[data-mfe="${tag}"]`)) {
      void whenReady();
      return () => {
        cancelled = true;
      };
    }

    // Never inject a bundle whose origin is not on the allow-list, even though the
    // server loader already rejected such entries — RemoteElement is the last gate
    // before first-party execution.
    if (!isAllowedScriptUrl(scriptUrl)) {
      console.error(`RemoteElement: refusing to load off-allow-list MFE bundle: ${scriptUrl}`);
      return () => {
        cancelled = true;
      };
    }

    const script = document.createElement("script");
    script.type = "module";
    script.src = scriptUrl;
    if (integrity) script.integrity = integrity;
    script.crossOrigin = "anonymous";
    script.dataset.mfe = tag;
    script.addEventListener("load", () => void whenReady());
    document.head.appendChild(script);
    return () => {
      cancelled = true;
    };
  }, [tag, scriptUrl, integrity]);

  // Create the element once it's defined and mount it into the host. Keyed on the
  // element identity only (ready, tag) so it is NOT torn down when inline
  // attributes/events object identities change on a parent re-render.
  useEffect(() => {
    const host = hostRef.current;
    if (!host || !ready) return;

    const element = document.createElement(tag);
    elementRef.current = element;
    host.appendChild(element);

    return () => {
      element.remove();
      elementRef.current = null;
    };
  }, [ready, tag]);

  // Apply attributes to the live element. Re-runs when attributes change, but reuses
  // the same element (no remount): unset removed attributes, set/update the rest.
  useEffect(() => {
    const element = elementRef.current;
    if (!element) return;

    const applied = new Set<string>();
    for (const [name, value] of Object.entries(attributes)) {
      element.setAttribute(name, typeof value === "object" ? JSON.stringify(value) : String(value));
      applied.add(name);
    }
    return () => {
      for (const name of applied) element.removeAttribute(name);
    };
  }, [ready, tag, attributes]);

  // Bind event handlers to the live element. detail is untrusted remote input; host
  // handlers must validate it. Rebinds when handlers change, again without remount.
  useEffect(() => {
    const element = elementRef.current;
    if (!element) return;

    const bound = Object.entries(events).map(([name, handler]): [string, EventListener] => {
      const listener: EventListener = (event) => handler((event as CustomEvent).detail);
      element.addEventListener(name, listener);
      return [name, listener];
    });
    return () => {
      bound.forEach(([name, listener]) => element.removeEventListener(name, listener));
    };
  }, [ready, tag, events]);

  return (
    <div ref={hostRef} className={className}>
      {ready ? null : fallback}
    </div>
  );
}
