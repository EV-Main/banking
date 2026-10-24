// The base of the in-page forms' shadow root: a wall against the host's inherited styles.
// Inherited properties cross a shadow boundary, and a host rule on the element's light DOM
// beats any `:host` rule, so the wall stands on the container inside the root, where no host
// selector can reach. The terms the
// person confirms are a security control: the host must not be able to change how they read.
//
// In a layer declared before the wallet sheet's, so every utility still wins over it.
//
// Pure and import-free so `node --test` runs it as is.

/** The attribute that names the container the wall stands on. */
export const FORM_CONTAINER_ATTR = "data-wallet-form";

export const SHADOW_BASE_CSS = `@layer wallet-base {
  :host { all: initial; display: block; }
  [${FORM_CONTAINER_ATTR}] {
    all: initial;
    display: block;
    color: var(--ink);
    font-family: var(--font-sans, ui-sans-serif, system-ui, sans-serif);
    font-size: 1rem;
    font-style: normal;
    font-weight: 400;
    line-height: 1.5;
    letter-spacing: normal;
    word-spacing: normal;
    text-align: start;
    text-transform: none;
    text-indent: 0;
    white-space: normal;
    -webkit-text-size-adjust: 100%;
  }
  [hidden]:where(:not([hidden="until-found"])) { display: none !important; }
}`;
