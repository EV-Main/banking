"use client";

import { usePathname, useRouter, useSearchParams } from "next/navigation";

import { SIGN_IN_PARAM } from "@/shared/lib/session-gate";
import { zonePathname } from "@/shared/config/base-path";

/** What an OAuth round trip that failed leaves on the page it returns to. */
const OAUTH_ERROR_PARAM = "auth_error";

/**
 * The sign-in dialog's state lives in the URL: `?login` opens it over whatever page
 * carries it, so a link from anywhere (the conductor's header, an email, `/login`) opens
 * the same one, and closing it leaves the page as it was.
 */
export function useSignInDialog() {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();
  const oauthError = params.get(OAUTH_ERROR_PARAM);

  /** The page, as it reads without the dialog. */
  function without(): URLSearchParams {
    const rest = new URLSearchParams(params);
    rest.delete(SIGN_IN_PARAM);
    rest.delete(OAUTH_ERROR_PARAM);
    return rest;
  }

  function href(query: URLSearchParams): string {
    const search = query.toString();
    return search ? `${pathname}?${search}` : pathname;
  }

  return {
    open: params.has(SIGN_IN_PARAM) || oauthError !== null,
    oauthError,
    /** Zone-relative, for the OAuth round trip to come back to. */
    returnTo: `${zonePathname(pathname)}${without().size ? `?${without()}` : ""}`,
    show() {
      const query = without();
      query.set(SIGN_IN_PARAM, "");
      router.replace(href(query), { scroll: false });
    },
    hide() {
      router.replace(href(without()), { scroll: false });
    },
    /** Signed in: load the page again, as the account's — a full load, so nothing a guest rendered survives. */
    reload() {
      window.location.assign(href(without()));
    },
  };
}
