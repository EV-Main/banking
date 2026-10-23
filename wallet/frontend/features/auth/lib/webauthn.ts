// The identity plane speaks WebAuthn as JSON with base64url binaries (webauthn-rs); the
// browser API wants ArrayBuffers. These convert at the edge, both ways, and nothing else.

type Json = Record<string, unknown>;

function toBuffer(value: string): ArrayBuffer {
  const base64 = value.replace(/-/g, "+").replace(/_/g, "/").padEnd(Math.ceil(value.length / 4) * 4, "=");
  return Uint8Array.from(atob(base64), (c) => c.charCodeAt(0)).buffer;
}

function toBase64url(buffer: ArrayBuffer): string {
  let binary = "";
  for (const byte of new Uint8Array(buffer)) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

const withIds = (list: unknown) => (Array.isArray(list) ? list.map((c: Json) => ({ ...c, id: toBuffer(c.id as string) })) : undefined);

export function creationOptions(options: { publicKey: Json }): CredentialCreationOptions {
  const pk = options.publicKey;
  const user = pk.user as Json;
  return {
    publicKey: {
      ...(pk as unknown as PublicKeyCredentialCreationOptions),
      challenge: toBuffer(pk.challenge as string),
      user: { ...(user as unknown as PublicKeyCredentialUserEntity), id: toBuffer(user.id as string) },
      excludeCredentials: withIds(pk.excludeCredentials) as PublicKeyCredentialDescriptor[] | undefined,
    },
  };
}

export function requestOptions(options: { publicKey: Json }): CredentialRequestOptions {
  const pk = options.publicKey;
  return {
    publicKey: {
      ...(pk as unknown as PublicKeyCredentialRequestOptions),
      challenge: toBuffer(pk.challenge as string),
      allowCredentials: withIds(pk.allowCredentials) as PublicKeyCredentialDescriptor[] | undefined,
    },
  };
}

export function registrationJson(credential: PublicKeyCredential): Json {
  const response = credential.response as AuthenticatorAttestationResponse;
  return {
    id: credential.id,
    rawId: toBase64url(credential.rawId),
    type: credential.type,
    response: { attestationObject: toBase64url(response.attestationObject), clientDataJSON: toBase64url(response.clientDataJSON) },
    extensions: {},
  };
}

export function assertionJson(credential: PublicKeyCredential): Json {
  const response = credential.response as AuthenticatorAssertionResponse;
  return {
    id: credential.id,
    rawId: toBase64url(credential.rawId),
    type: credential.type,
    response: {
      authenticatorData: toBase64url(response.authenticatorData),
      clientDataJSON: toBase64url(response.clientDataJSON),
      signature: toBase64url(response.signature),
      userHandle: response.userHandle ? toBase64url(response.userHandle) : null,
    },
    extensions: {},
  };
}

/** Whether this browser can hold a passkey at all. */
export function passkeysSupported(): boolean {
  return typeof window !== "undefined" && typeof window.PublicKeyCredential === "function";
}
