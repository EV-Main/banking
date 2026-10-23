// A wire vocabulary in the reader's language. The words are built per call from literal
// `t(key, English)` pairs — the catalogue is extracted from those, so a key assembled at
// runtime would have no English to extract — and looked up here by the value the plane sent.

/**
 * The word for a wire value, or `undefined` when this build has no name for it, so the caller
 * picks the fallback (usually the bare wire word, never a raw catalogue key). `Object.hasOwn`,
 * not a bare index: a value such as `toString` must not resolve up the prototype chain.
 */
export function wordFor(words: Readonly<Record<string, string>>, wire: string | null | undefined): string | undefined {
  return wire != null && Object.hasOwn(words, wire) ? words[wire] : undefined;
}
