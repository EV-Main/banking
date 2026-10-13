import type { Locale } from "@evinvest/i18n";

/**
 * The documents the cabinet may point a reader at, and where each one lives.
 *
 * Every entry but `sourceCode` is a page on the public site (the conductor), which shares
 * the cabinet's origin and locale prefix — so a path here is site-root, locale-less, and
 * prefixed at render by {@link siteDocumentHref}. Constants rather than `config`: which pages the site
 * publishes is a fact about the site, not a deployment knob.
 *
 * `undefined` means the page does not exist yet and the row MUST NOT render — a link to a
 * page that is not there is worse than no link (banking #385). Terms, privacy and a risk
 * disclosure are the ones still missing: the site has no such pages, so they are listed
 * here as absent rather than pointed at `#`.
 */
export interface SiteDocuments {
  /** The fund's whitepaper — the site's `/publications/whitepaper`. */
  readonly whitepaper: `/${string}` | undefined;
  /**
   * Where the client-facing products — this cabinet and the public site — are developed in
   * the open. The one entry that is not a site page but another origin (GitHub), so it is
   * a full URL and the row that renders it opens a new tab.
   */
  readonly sourceCode: `https://${string}`;
  /** TODO(#385): the site publishes no terms page yet. */
  readonly terms: `/${string}` | undefined;
  /** TODO(#385): the site publishes no privacy page yet. */
  readonly privacy: `/${string}` | undefined;
  /** TODO(#385): the site publishes no risk disclosure yet; the cabinet states the risk itself. */
  readonly riskDisclosure: `/${string}` | undefined;
}

export const SITE_DOCUMENTS: SiteDocuments = {
  whitepaper: "/publications/whitepaper",
  sourceCode: "https://github.com/EV-Main",
  terms: undefined,
  privacy: undefined,
  riskDisclosure: undefined,
};

/** A site page for the reader's locale: `/ru/publications/whitepaper`. */
export const siteDocumentHref = (locale: Locale, path: `/${string}`): string => `/${locale}${path}`;
