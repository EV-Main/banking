import { flushSync } from "react-dom";
import { createRoot, type Root } from "react-dom/client";

import { AccountChip, HOST_ATTRS, HostAccountChip, isHostMode, readHostProps } from "@/features/account-chip";

import { stylesheetHref, whenStylesheetReady } from "./stylesheet";

// Self-registering ESM entry for the <mfe-cabinet-account-chip> element remote. Hosts inject
// this bundle (site_conductor's mfe-registry → `/cabinet/mfe/account-chip.js`; the
// Service-Arb panel by its own loader) and place the element in their header. React renders
// into the element's LIGHT DOM so the host's tokens and fonts cascade in; the chip's own
// utilities ship in a sibling stylesheet, read and inlined as `@layer reamfe` (stylesheet.ts).
//
// Two modes, picked by `data-mode`:
//   • cabinet (default) — the chip reads the cabinet session, profile and identity cache.
//   • host (`data-mode="host"`) — a host on another origin, with its own session, passes the
//     identity, links and service groups as attributes; the chip fetches nothing and turns
//     Sign out into a `signout` event for the host to act on.
// Every attribute is observed: a change re-renders the same root, so open state survives.

const TAG = "mfe-cabinet-account-chip";

// The chip's final box with its name still unknown (avatar 34 + gaps 2×10 + bar 96 + chevron
// 16 + padding 2×6, by 34 + padding 2×4). Held, invisible, until the stylesheet applies, so the
// host's header neither collapses nor jumps under an unstyled first frame.
const RESERVE = { display: "inline-block", width: "178px", height: "42px", visibility: "hidden" } as const;

class AccountChipElement extends HTMLElement {
  /** Lets a host's loader tell this bundle from one without host mode (no static: v1). */
  static apiVersion = 2;
  static observedAttributes = Object.values(HOST_ATTRS);
  #root?: Root;
  #styled = false;

  connectedCallback() {
    const root = createRoot(this);
    this.#root = root;
    this.#hold();
    void whenStylesheetReady(TAG, stylesheetHref()).then(() => {
      // Disconnected (or reconnected under a new root) while waiting.
      if (this.#root !== root) return;
      this.#styled = true;
      // Committed synchronously so the reserve is released in the same frame the chip paints.
      flushSync(() => this.#render());
      this.#release();
    });
  }

  attributeChangedCallback() {
    // Fires before connect for attributes set on a detached element, and before the sheet
    // lands; both renders happen once it has.
    if (this.#styled) this.#render();
  }

  disconnectedCallback() {
    this.#root?.unmount();
    this.#root = undefined;
    this.#styled = false;
  }

  #hold() {
    for (const [key, value] of Object.entries(RESERVE)) this.style.setProperty(key, value);
  }

  #release() {
    for (const key of Object.keys(RESERVE)) this.style.removeProperty(key);
  }

  #signOut = () => {
    // composed: a host that wraps the element in its own shadow root still hears it.
    this.dispatchEvent(new CustomEvent("signout", { bubbles: true, composed: true }));
  };

  #render() {
    const read = (name: string) => this.getAttribute(name);
    this.#root?.render(
      isHostMode(read) ? (
        <HostAccountChip {...readHostProps(read)} onSignOut={this.#signOut} />
      ) : (
        <AccountChip intent={read(HOST_ATTRS.intent)} returnTo={read(HOST_ATTRS.returnTo)} />
      ),
    );
  }
}

if (!customElements.get(TAG)) customElements.define(TAG, AccountChipElement);
