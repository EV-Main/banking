// Run with `npm run test`.
//
// A money-out on the wallet's own page, driven as the person drives it: Pay, then Confirm on
// the BFF's terms. What is pinned: an intent with a `min_aal` is never confirmed without
// concierge's `requestProof` for that intent's hash, and the confirm carries the proof; a
// dismissed dialog sends nothing; a proof that comes back after the intent ran out confirms
// a new intent for the same payment, asking again, and never the stale one.
//
// The page renders on the stand-in document; the network is a fake `fetch`, and the
// concierge hook is a stand-in whose `requestProof` the test answers.
import assert from "node:assert/strict";
import test, { after, afterEach, before, beforeEach } from "node:test";

import { bundle } from "../../../shared/__tests__/bundle.ts";
import { FakeDocument, FakeElement, click, descendants, installDom } from "../../../shared/__tests__/fake-dom.ts";

type Loaded = Pick<typeof import("react"), "act" | "createElement"> &
  Pick<typeof import("react-dom/client"), "createRoot"> &
  Pick<typeof import("@evinvest/i18n/react"), "I18nProvider"> &
  Pick<typeof import("./pay-invoice-action-view.tsx"), "PayInvoiceActionView">;

interface Asked {
  hash: string;
  aal: number;
  answer: (proof: string | null) => void;
}

interface Sent {
  path: string;
  body: unknown;
}

const stage = globalThis as unknown as { asked: Asked[]; finished: unknown[][] };

const STUBS = {
  "@evinvest/concierge/react": `
    const stage = globalThis;
    export function useStepup() {
      return {
        open: false,
        requestProof: (hash, aal) => new Promise((answer) => stage.asked.push({ hash, aal, answer })),
        dialogProps: { open: false, request: null, onOpenChange() {}, onProof() {} },
      };
    }
    export function StepupDialog() { return null; }
  `,
  "../model/stepup-source": `export const useStepupSource = () => ({ status: "ready", scriptUrl: "/api/auth/mfe/stepup.js" });`,
  "@/features/wallet-action": `
    export const ActionFrame = ({ children }) => children;
    export const ActionCancel = () => null;
    export const useWalletAction = () => ({
      params: { invoice: "inv1", scope: "invest", opener: null, returnTo: null },
      finished: null,
      finish: (...args) => globalThis.finished.push(args),
    });
  `,
  // The wallet screen chrome pulls in Next's router, which has no place here.
  "@/views/wallet/ui/wallet-chrome": `export const WALLET_CARD = "card";`,
  "@/entities/invoice/api/invoice-client": `
    export const fetchPayableInvoice = async () => ({
      invoice_id: "inv1", scope: "invest", display_name: "Invest", amount: "25", memo: "", state: "open",
      available_main: "100", available_scope: "0",
    });
  `,
};

const doc = new FakeDocument();
let uninstall: () => void;
let page: Loaded;
let container: FakeElement;
let unmount: () => Promise<void>;
let sent: Sent[];
let intents: { id: string; hash: string; expiresAt: number }[];
let realNow: () => number;

before(async () => {
  uninstall = installDom(doc);
  page = (await bundle(`export { PayInvoiceActionView } from "@/views/wallet/ui/pay-invoice-action-view";\nexport { I18nProvider } from "@evinvest/i18n/react";`, STUBS)) as Loaded;
});

after(() => uninstall());

const intentBody = (id: string, hash: string, expiresAt: number) => ({
  intent_id: id,
  intent_hash: hash,
  expires_at: expiresAt,
  summary: { amount: "25", fee: null, net: null, recipient_full: "Invest", network: null, scope: "main", host_origin: "https://cabinet.example", mandate: null },
  min_aal: 2,
  new_recipient: null,
  release_at: null,
});

beforeEach(() => {
  realNow = Date.now;
  stage.asked = [];
  stage.finished = [];
  sent = [];
  intents = [];
  let created = 0;
  Reflect.set(globalThis, "fetch", async (url: string, init: RequestInit = {}) => {
    const path = new URL(url, "https://cabinet.example").pathname;
    const body: unknown = typeof init.body === "string" ? JSON.parse(init.body) : null;
    sent.push({ path, body });
    const reply = (status: number, json: unknown) => ({ status, ok: status < 300, json: async () => json });
    if (path.endsWith("/csrf")) return reply(200, { csrf: "tok", expires_at: 9_999_999_999, kinds: ["withdraw", "pay", "mandate", "approve_payment"] });
    if (path.endsWith("/intents")) {
      created++;
      const intent = { id: String(created).repeat(32), hash: String(created).repeat(64), expiresAt: Math.floor(Date.now() / 1000) + 120 };
      intents.push(intent);
      return reply(200, intentBody(intent.id, intent.hash, intent.expiresAt));
    }
    if (path.endsWith("/confirm")) {
      const id = path.split("/").at(-2);
      return reply(200, { intent_id: id, status: "executed", stepup_method: "aal2", result: { invoice_id: "inv1", state: "paid", amount: "25" } });
    }
    throw new Error(`unexpected request ${path}`);
  });
  container = doc.body.appendChild(doc.createElement("div"));
});

afterEach(async () => {
  Date.now = realNow;
  await unmount();
  doc.body.removeChild(container);
});

/** Runs `step`, then lets every pending answer and render land. */
async function settle(step: () => void = () => undefined): Promise<void> {
  await page.act(async () => {
    step();
    for (let i = 0; i < 20; i++) await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

/** Concierge's element answers the `at`-th ask. */
async function answer(at: number, proof: string | null): Promise<void> {
  const asked = stage.asked[at];
  assert.ok(asked, `no ask #${at}`);
  await settle(() => asked.answer(proof));
}

async function render(): Promise<void> {
  const root = page.createRoot(container as unknown as Element);
  unmount = async () => {
    await page.act(() => root.unmount());
  };
  await settle(() => root.render(page.createElement(page.I18nProvider, { locale: "en", messages: {}, children: page.createElement(page.PayInvoiceActionView) })));
}

async function press(label: string): Promise<void> {
  const button = descendants(container).find((el) => el.tagName === "BUTTON" && el.textContent.trim() === label && !el.hasAttribute("disabled"));
  assert.ok(button, `no enabled "${label}" button in ${container.outerHTML}`);
  await settle(() => void click(container, button));
}

const confirms = () => sent.filter((call) => call.path.endsWith("/confirm"));

test("a payment that asks for assurance confirms only with the proof concierge gave for its hash", async () => {
  await render();
  await press("Pay");
  assert.deepEqual(sent.find((call) => call.path.endsWith("/intents"))?.body, { kind: "pay", params: { invoice_id: "inv1", pay_from: "main" } });

  await press("Confirm");
  assert.equal(stage.asked.length, 1);
  assert.deepEqual([stage.asked[0]?.hash, stage.asked[0]?.aal], [intents[0]?.hash, 2]);
  assert.deepEqual(confirms(), [], "confirmed before the proof came back");

  await answer(0, "proof-1");
  assert.deepEqual(confirms(), [{ path: `/cabinet/api/wallet/intents/${intents[0]?.id}/confirm`, body: { proof: "proof-1" } }]);
  assert.deepEqual(stage.finished, [["done", "inv1"]]);
});

test("a dismissed step-up sends nothing and leaves Confirm on the same terms", async () => {
  await render();
  await press("Pay");
  await press("Confirm");

  await answer(0, null);
  assert.deepEqual(confirms(), []);
  assert.equal(intents.length, 1);
  await press("Confirm");
  assert.equal(stage.asked[1]?.hash, intents[0]?.hash);
});

test("a proof that outlived its intent asks again for a new one and never confirms the stale one", async () => {
  await render();
  await press("Pay");
  await press("Confirm");

  const start = Date.now();
  Date.now = () => start + 121_000;
  await answer(0, "late-proof");

  assert.equal(intents.length, 2, "the same payment was not created again");
  assert.deepEqual(sent.filter((call) => call.path.endsWith("/intents")).map((call) => call.body), [
    { kind: "pay", params: { invoice_id: "inv1", pay_from: "main" } },
    { kind: "pay", params: { invoice_id: "inv1", pay_from: "main" } },
  ]);
  assert.deepEqual(confirms(), []);
  assert.equal(stage.asked[1]?.hash, intents[1]?.hash, "unchanged terms ask for the new intent's proof at once");

  await answer(1, "proof-2");
  assert.deepEqual(confirms(), [{ path: `/cabinet/api/wallet/intents/${intents[1]?.id}/confirm`, body: { proof: "proof-2" } }]);
});
