import { useCallback, useEffect, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";

import { PortalProvider } from "@evinvest/uikit";

import { FORM_CONTAINER_ATTR } from "../lib/shadow-base";
import { shadowBaseSheet, shadowSheet } from "../lib/shadow-styles";

// Session replay (Sentry) and autocapture (PostHog) skip what carries these. Both the root's
// host and the form container wear them: a recorder that cannot enter the closed root still
// sees the host, and one that can still meets the container.
const PRIVATE_CLASS = "ph-no-capture";
const PRIVATE_ATTRS = { "data-ph-no-capture": "", "data-sentry-block": "" } as const;
const CONTAINER_ATTRS = { ...PRIVATE_ATTRS, [FORM_CONTAINER_ATTR]: "" };

// `attachShadow` runs once per element, and a closed root cannot be read back from it.
const roots = new WeakMap<Element, ShadowRoot>();

function shadowOf(host: Element): ShadowRoot {
  const known = roots.get(host);
  if (known) return known;
  const root = host.attachShadow({ mode: "closed" });
  roots.set(host, root);
  return root;
}

/**
 * A closed shadow root around the money-action forms only; the wallet card stays in light
 * DOM. The host page's scripts and styles cannot reach in, and its recorders are told to stay
 * out. Only inline uikit parts render inside: the kit's overlays dismiss and trap focus
 * through `document`, which cannot see into a closed root, so none is used here and the
 * PortalProvider keeps any that slips in inside the root instead of on the host's body.
 */
export function ActionRoot({ sheetHref, children }: { sheetHref: string; children: ReactNode }) {
  const [shadow, setShadow] = useState<ShadowRoot | null>(null);
  const [container, setContainer] = useState<HTMLDivElement | null>(null);
  const [styled, setStyled] = useState(false);
  const host = useCallback((node: HTMLDivElement | null) => {
    if (node) setShadow(shadowOf(node));
  }, []);

  // `autoFocus` fires while the container is still `hidden`, and a browser ignores focus on
  // what it does not render; so the form takes focus once it shows. Its first field, else
  // the region itself, when a form has no field yet (the withdrawal still reading its rails).
  useEffect(() => {
    if (!styled || !container) return;
    const target = container.querySelector<HTMLElement>("input:not([disabled])") ?? container.querySelector<HTMLElement>('[role="region"]');
    target?.focus();
  }, [styled, container]);

  useEffect(() => {
    if (!shadow) return;
    let live = true;
    let link: HTMLLinkElement | null = null;
    void shadowSheet(sheetHref).then((sheet) => {
      if (!live) return;
      if (sheet) {
        shadow.adoptedStyleSheets = [shadowBaseSheet(), sheet];
        setStyled(true);
        return;
      }
      // Unreadable as text: the plain sheet still styles the parts, without the `:host` tokens.
      shadow.adoptedStyleSheets = [shadowBaseSheet()];
      link = document.createElement("link");
      link.rel = "stylesheet";
      link.href = sheetHref;
      link.addEventListener("load", () => live && setStyled(true));
      link.addEventListener("error", () => live && setStyled(true));
      shadow.appendChild(link);
    });
    return () => {
      live = false;
      link?.remove();
    };
  }, [shadow, sheetHref]);

  return (
    <div ref={host} className={PRIVATE_CLASS} {...PRIVATE_ATTRS} aria-busy={!styled}>
      {shadow &&
        createPortal(
          <div ref={setContainer} className={`${PRIVATE_CLASS} text-ink`} {...CONTAINER_ATTRS} hidden={!styled}>
            {container && <PortalProvider container={container}>{children}</PortalProvider>}
          </div>,
          shadow,
        )}
    </div>
  );
}
