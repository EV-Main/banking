import { flushSync } from "react-dom";
import { createRoot, type Root } from "react-dom/client";

import { AccountChip, HOST_ATTRS, HostAccountChip, isHostMode, readHostProps } from "@/features/account-chip";
import { CHIP_TAG } from "@/shared/config/chip-element";

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
// The element sets `data-ready` itself once the chip paints with its styles (removed on
// disconnect); hosts wait on that rather than on the reserve's inline style going away.
//
// Events the element dispatches (both modes; bubbles, composed):
//   • `signout` (host mode) — Sign out was picked; the host ends its own session.
//   • `ev:navigate` (cancelable) — a plain same-origin click on a menu link: a primary
//     button with no modifier keys, no `target`/`download`, not already prevented. `detail`
//     is `{ href }`, the destination as path + query + hash. A host that routes the path
//     itself calls `preventDefault()` and pushes it, and the chip cancels the click; left
//     alone, the browser follows the link (a full load). Sign out is not a link and never
//     sends it. Additive: hosts without a listener behave as before, so apiVersion stays 2.
//     (features/account-chip/lib/navigate-event.ts)

const TAG = CHIP_TAG;

// Until the stylesheet applies the chip is invisible, so an unstyled first frame never paints.
// Its box is the host's when the host gave the element one — a class on the element itself
// (the cabinet zone's shell: flex, a fixed width below `sm`, `hidden` when signed out) or a
// flex/grid parent: an inline display or size here would beat those, and in the zone it
// grew the row 42 → 48 px for half a second. Only an element left in inline flow, which
// would collapse to nothing, gets the chip's own final box with its name still unknown
// (avatar 34 + gaps 2×10 + bar 96 + chevron 16 + padding 2×6, by 34 + padding 2×4).
const HIDDEN = { visibility: "hidden" } as const;
const RESERVE = { ...HIDDEN, display: "inline-block", width: "178px", height: "42px" } as const;

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
    this.removeAttribute("data-ready");
    // A reserve left inline would read as the host's own display on the next connect.
    for (const key of Object.keys(RESERVE)) this.style.removeProperty(key);
  }

  #hold() {
    // Read before anything is set inline, so it is the host's display. One forced style
    // recalc per connect; the host's render-blocking sheets have applied by the time a
    // module script runs.
    // An empty value (no browsing context to style it) cannot vouch for a host box either.
    const hostBox = (getComputedStyle(this).display || "inline") !== "inline";
    for (const [key, value] of Object.entries(hostBox ? HIDDEN : RESERVE)) this.style.setProperty(key, value);
  }

  #release() {
    // Every key either branch may have set; removing one that was not set is a no-op.
    for (const key of Object.keys(RESERVE)) this.style.removeProperty(key);
    // The explicit "painted" signal for hosts (`[data-ready]`), instead of watching for the
    // inline visibility to disappear.
    this.toggleAttribute("data-ready", true);
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
