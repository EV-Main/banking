// The base of the in-page forms' shadow root.
//
// A wall against the host's inherited styles. Inherited properties cross a shadow boundary,
// and a host rule on the element's light DOM beats any `:host` rule, so the wall stands on the
// container inside the root, where no host selector reaches. The terms the person confirms
// are a security control: the host must not be able to change how they read.
//
// The reset a page's preflight gives everything outside the root. A shadow root gets none:
// a `w-full` field was content-box and poked out of its card, and controls drew in the
// browser's own font.
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
  *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; border: 0 solid; }
  button, input, select, textarea { font: inherit; letter-spacing: inherit; color: inherit; background-color: transparent; border-radius: 0; }
  button, [role="button"] { cursor: pointer; }
  :disabled { cursor: default; }
  input::placeholder { opacity: 1; color: color-mix(in oklab, currentcolor 50%, transparent); }
  ol, ul, menu { list-style: none; }
  svg { display: block; vertical-align: middle; }
  [hidden]:where(:not([hidden="until-found"])) { display: none !important; }
}`;
