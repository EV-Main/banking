import { localeOfElement, type Locale, type Messages } from "@evinvest/i18n";
import { I18nProvider } from "@evinvest/i18n/react";
import type { ReactNode } from "react";
import { flushSync } from "react-dom";
import { createRoot, type Root } from "react-dom/client";

import { InPageAction, PayInvoiceButton, WalletPanel, actionId, actionMode, offerWalletAction, readScope, walletActionHref, type WalletAction } from "@/features/wallet-mfe";

import { stylesheetHref, whenStylesheetReady } from "../account-chip/stylesheet";

// Self-registering ESM entry for banking's wallet element remotes, served from
// `https://evinvest.ltd/wallet/mfe/` and mounted by the invest cabinet, the SA panel and
// admin through `@evinvest/wallet` (banking `sdk/wallet` — the host side of this contract;
// tags, attributes and events must stay in step with it). Two elements, one bundle:
//
//   • <mfe-banking-wallet scope?> — balances and recent history. With `scope` (a scope
//     host) it shows main and the compartments; without, the full split.
//   • <mfe-banking-pay-invoice invoice scope?> — the pay button for one invoice.
//
// By default neither moves money. Every money action is a page on the wallet origin, offered
// to the host as a cancelable `ev:wallet-action` (`detail: { href }`); a host that takes it
// opens a popup and hears the result by postMessage. Left alone, the element navigates there.
// A build with `__WALLET_IN_PAGE__` runs the action on a scope host inside the element
// instead, in a closed shadow root, through the BFF's intent routes (docs/WALLET-INPAGE-SPEC.md).
// Light DOM, like the account chip: the host's tokens and fonts cascade in, and the
// element's own utilities arrive as `@layer reamfe`, below every host layer.

// esbuild `define` (mfe/build.mjs): the element's own keys cut from the wallet host catalogues,
// already resolved per locale — not `messagesFor()`, which would ship all five catalogues.
declare const __WALLET_MESSAGES__: Partial<Record<Locale, Messages>>;
// esbuild `define` (mfe/build.mjs, `WALLET_IN_PAGE=1`); false unless the build turns it on.
declare const __WALLET_IN_PAGE__: boolean;

const API_VERSION = 2;
const WALLET_TAG = "mfe-banking-wallet";
const PAY_TAG = "mfe-banking-pay-invoice";
// The bundle's origin is the wallet's: its BFF and its action pages live beside it.
const ORIGIN = new URL(import.meta.url).origin;
const SHEET = stylesheetHref("wallet.css");

let roots = 0;

interface OpenAction {
  action: WalletAction;
  invoice: string | null;
  scope: string;
}

// Null without the flag, so esbuild drops the forms, the intent client and the shadow-root
// styling from the bundle rather than shipping them unreachable.
const inPageForm = __WALLET_IN_PAGE__
  ? (open: OpenAction, close: () => void, offer: () => void) => (
      <InPageAction
        key={`${open.action}:${open.invoice ?? ""}`}
        origin={ORIGIN}
        sheetHref={SHEET}
        scope={open.scope}
        action={open.action}
        invoice={open.invoice}
        request={null}
        onClose={close}
        onFallback={() => {
          offer();
          close();
        }}
      />
    )
  : null;

abstract class WalletElementBase extends HTMLElement {
  static apiVersion = API_VERSION;
  static observedAttributes = ["scope", "invoice"];
  #root?: Root;
  #styled = false;
  #open: OpenAction | null = null;

  protected abstract view(): ReactNode;

  connectedCallback() {
    // Unique per element: `useId` is unique per root only, and a page may hold several.
    const root = createRoot(this, { identifierPrefix: `evw${++roots}-` });
    this.#root = root;
    this.style.setProperty("visibility", "hidden");
    void whenStylesheetReady(WALLET_TAG, SHEET).then(() => {
      if (this.#root !== root) return;
      this.#styled = true;
      flushSync(() => this.#render());
      this.style.removeProperty("visibility");
      this.toggleAttribute("data-ready", true);
    });
  }

  attributeChangedCallback() {
    // An open form holds the scope and invoice it opened with; new attributes start over.
    this.#open = null;
    if (this.#styled) this.#render();
  }

  disconnectedCallback() {
    this.#root?.unmount();
    this.#root = undefined;
    this.#styled = false;
    this.#open = null;
    this.removeAttribute("data-ready");
    this.style.removeProperty("visibility");
  }

  protected scope(): string | null {
    return readScope(this.getAttribute("scope"));
  }

  protected act(action: WalletAction, invoice: string | null = null) {
    const scope = this.scope();
    if (inPageForm !== null && scope !== null && actionMode(__WALLET_IN_PAGE__, scope) === "in_page") {
      this.#open = { action, invoice, scope };
      this.#render();
      return;
    }
    this.#offer(action, invoice);
  }

  #offer(action: WalletAction, invoice: string | null) {
    const href = walletActionHref(ORIGIN, action, { scope: this.scope(), invoice });
    if (offerWalletAction(this, href)) return;
    // Nobody took it: on evinvest.ltd itself that is the design (actions run in place);
    // on a host without the SDK it is still the safe way. Coming back is the page's call —
    // it honours `return_to` only on its own origin or a scope's registered paths.
    const url = new URL(href);
    url.searchParams.set("return_to", window.location.origin + window.location.pathname);
    window.location.assign(url.href);
  }

  #close() {
    this.#open = null;
    this.#render();
  }

  #offerOpen() {
    const open = this.#open;
    if (open) this.#offer(open.action, open.invoice);
  }

  #render() {
    const locale = localeOfElement(this);
    this.#root?.render(
      <I18nProvider locale={locale} messages={__WALLET_MESSAGES__[locale] ?? {}}>
        {this.view()}
        {this.#open && inPageForm?.(this.#open, () => this.#close(), () => this.#offerOpen())}
      </I18nProvider>,
    );
  }
}

class WalletElement extends WalletElementBase {
  protected view() {
    return <WalletPanel origin={ORIGIN} scope={this.scope()} onAction={(action) => this.act(action)} />;
  }
}

class PayInvoiceElement extends WalletElementBase {
  protected view() {
    const invoice = actionId(this.getAttribute("invoice"));
    return <PayInvoiceButton invoice={invoice} onPay={() => this.act("pay", invoice)} />;
  }
}

if (!customElements.get(WALLET_TAG)) customElements.define(WALLET_TAG, WalletElement);
if (!customElements.get(PAY_TAG)) customElements.define(PAY_TAG, PayInvoiceElement);
