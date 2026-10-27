import { localeOfElement, type Locale, type Messages } from "@evinvest/i18n";
import { I18nProvider } from "@evinvest/i18n/react";
import type { ReactNode } from "react";
import { flushSync } from "react-dom";
import { createRoot, type Root } from "react-dom/client";

import {
  InPageAction,
  MandateButton,
  PayInvoiceButton,
  WalletPanel,
  actionId,
  actionMode,
  dispatchWalletResult,
  enabledKinds,
  offerWalletAction,
  readScope,
  walletActionHref,
  walletResultDetail,
  type ClosedAction,
  type WalletAction,
} from "@/features/wallet-mfe";
import type { IntentKind } from "@/features/wallet-intent";

import { stylesheetHref, whenStylesheetReady } from "../account-chip/stylesheet";

// Self-registering ESM entry for banking's wallet element remotes, served from
// `https://evinvest.ltd/wallet/mfe/` and mounted by the invest cabinet, the SA panel and
// admin through `@evinvest/wallet` (banking `sdk/wallet` — the host side of this contract;
// tags, attributes and events must stay in step with it). Three elements, one bundle:
//
//   • <mfe-banking-wallet scope? actions?> — balances and recent history. With `scope` (a
//     scope host) it shows main and the compartments; without, the full split.
//   • <mfe-banking-pay-invoice invoice scope? actions?> — the pay button for one invoice.
//   • <mfe-banking-mandate request scope? actions?> — the button for one mandate request.
//
// By default none moves money. Every money action is a page on the wallet origin, offered
// to the host as a cancelable `ev:wallet-action` (`detail: { href }`); a host that takes it
// opens a popup and hears the result by postMessage. Left alone, the element navigates there.
// An action runs inside the element instead, in a closed shadow root, through the intent
// routes (docs/WALLET-INPAGE-SPEC.md), only when both hold: the host opted in with
// `actions="inline"`, and the BFF lists the kind for the host page (`GET /api/wallet/csrf` →
// `kinds`). Turning a kind off is a server env change, not a release. When the in-page form
// closes, the element dispatches `ev:wallet-result` (`detail: { kind, outcome, reference? }`,
// the popup's words); a fallback to the popup dispatches nothing, as the popup reports itself.
// Light DOM, like the account chip: the host's tokens and fonts cascade in, and the
// element's own utilities arrive as `@layer reamfe`, below every host layer.

// esbuild `define` (mfe/build.mjs): the element's own keys cut from the wallet host catalogues,
// already resolved per locale — not `messagesFor()`, which would ship all five catalogues.
declare const __WALLET_MESSAGES__: Partial<Record<Locale, Messages>>;

// 3: the `actions` opt-in, `ev:wallet-result`, the mandate element. Hosts check `>=`.
const API_VERSION = 3;
const WALLET_TAG = "mfe-banking-wallet";
const PAY_TAG = "mfe-banking-pay-invoice";
const MANDATE_TAG = "mfe-banking-mandate";
// The bundle's origin is the wallet's: its BFF and its action pages live beside it.
const ORIGIN = new URL(import.meta.url).origin;
const SHEET = stylesheetHref("wallet.css");

let roots = 0;

/** The ids an action carries besides the scope, each already passed through `actionId`. */
interface ActionParams {
  invoice: string | null;
  request: string | null;
}

interface OpenAction extends ActionParams {
  action: WalletAction;
  scope: string | null;
  /** What had focus when the form opened, so closing it puts the reader back there. */
  trigger: HTMLElement | null;
}

/** The focused element, through any open shadow roots the host wraps the element in. */
function focusedElement(): HTMLElement | null {
  let active = document.activeElement;
  while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
  return active instanceof HTMLElement ? active : null;
}

function inPageForm(open: OpenAction, finish: (closed: ClosedAction) => void, fallback: () => void) {
  return (
    <InPageAction
      key={`${open.action}:${open.invoice ?? ""}:${open.request ?? ""}`}
      origin={ORIGIN}
      sheetHref={SHEET}
      scope={open.scope}
      action={open.action}
      invoice={open.invoice}
      request={open.request}
      onClose={finish}
      onFallback={fallback}
    />
  );
}

abstract class WalletElementBase extends HTMLElement {
  static apiVersion = API_VERSION;
  static observedAttributes = ["scope", "invoice", "request", "actions"];
  #root?: Root;
  #styled = false;
  #open: OpenAction | null = null;
  // Null until the BFF answers. A click before that is an offer: `act` never waits, because a
  // host opens its popup inside the click and a popup opened after an await is blocked.
  #kinds: IntentKind[] | null = null;

  protected abstract view(): ReactNode;

  connectedCallback() {
    // Unique per element: `useId` is unique per root only, and a page may hold several.
    const root = createRoot(this, { identifierPrefix: `evw${++roots}-` });
    this.#root = root;
    void enabledKinds(ORIGIN).then((kinds) => {
      if (this.#root === root) this.#kinds = kinds;
    });
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
    // An open form holds the scope and ids it opened with; new attributes start over, and the
    // form they drop reports nothing: it did not end, the host replaced it.
    this.#open = null;
    if (this.#styled) this.#render();
  }

  disconnectedCallback() {
    this.#root?.unmount();
    this.#root = undefined;
    this.#styled = false;
    this.#open = null;
    this.#kinds = null;
    this.removeAttribute("data-ready");
    this.style.removeProperty("visibility");
  }

  protected scope(): string | null {
    return readScope(this.getAttribute("scope"));
  }

  protected act(action: WalletAction, ids: Partial<ActionParams> = {}) {
    const scope = this.scope();
    const params: ActionParams = { invoice: ids.invoice ?? null, request: ids.request ?? null };
    if (actionMode(this.getAttribute("actions"), this.#kinds, action, scope) === "in_page") {
      this.#open = { action, ...params, scope, trigger: focusedElement() };
      this.#render();
      return;
    }
    this.#offer(action, params);
  }

  #offer(action: WalletAction, { invoice, request }: ActionParams) {
    const href = walletActionHref(ORIGIN, action, { scope: this.scope(), invoice, request });
    if (offerWalletAction(this, href)) return;
    // Nobody took it: on evinvest.ltd itself that is the design (actions run in place);
    // on a host without the SDK it is still the safe way. Coming back is the page's call —
    // it honours `return_to` only on its own origin or a scope's registered paths.
    const url = new URL(href);
    url.searchParams.set("return_to", window.location.origin + window.location.pathname);
    window.location.assign(url.href);
  }

  /** False when `open` is no longer the form on screen: a second click on Close, a late callback. */
  #close(open: OpenAction): boolean {
    if (this.#open !== open) return false;
    this.#open = null;
    this.#render();
    if (open.trigger?.isConnected) open.trigger.focus();
    return true;
  }

  // Once per form: the host hears one result for one action.
  #finish(open: OpenAction, closed: ClosedAction) {
    if (this.#close(open)) dispatchWalletResult(this, walletResultDetail(open.action, closed));
  }

  // The popup reports this action itself; a result from here too would be a second one.
  #fallback(open: OpenAction) {
    if (this.#open !== open) return;
    this.#offer(open.action, open);
    this.#close(open);
  }

  #render() {
    const locale = localeOfElement(this);
    const open = this.#open;
    this.#root?.render(
      <I18nProvider locale={locale} messages={__WALLET_MESSAGES__[locale] ?? {}}>
        {this.view()}
        {open && inPageForm(open, (closed) => this.#finish(open, closed), () => this.#fallback(open))}
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
    return <PayInvoiceButton invoice={invoice} onPay={() => this.act("pay", { invoice })} />;
  }
}

class MandateElement extends WalletElementBase {
  protected view() {
    const request = actionId(this.getAttribute("request"));
    return <MandateButton request={request} onReview={() => this.act("mandate", { request })} />;
  }
}

if (!customElements.get(WALLET_TAG)) customElements.define(WALLET_TAG, WalletElement);
if (!customElements.get(PAY_TAG)) customElements.define(PAY_TAG, PayInvoiceElement);
if (!customElements.get(MANDATE_TAG)) customElements.define(MANDATE_TAG, MandateElement);
