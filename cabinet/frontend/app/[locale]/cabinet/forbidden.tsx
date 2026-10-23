import { currentLocale } from "@/shared/config/locale";
import { LocalisedStatus } from "@/views/status";

// Next's `forbidden.tsx` file convention (experimental `authInterrupts`, enabled in
// next.config.ts): rendered with a 403 whenever the `forbidden()` interrupt is
// invoked from a Server Component or Route Handler. It is NOT a browsable
// `/forbidden` route.
//
// The flag matters — without it next-app-loader never resolves this file and the
// page is silently ignored, which is how it sat here doing nothing until now.
//
// The route gate is client-side (the principal is read from the shell's
// `/api/auth/session` in the browser), so it renders the 403 surface directly
// instead of calling this. This file backs the server half: anything that learns
// "signed in, but not allowed" while rendering on the server.
export default async function ForbiddenPage() {
  return <LocalisedStatus kind="forbidden" locale={await currentLocale()} />;
}
