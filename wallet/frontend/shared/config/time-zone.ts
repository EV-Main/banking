import "server-only";

import { cookies } from "next/headers";

import { COOKIES } from "@/shared/config/cookies";
import { isTimeZone } from "@/shared/lib/time-zone-name";

/**
 * The zone this request renders times in: the one the reader's browser stored, else UTC.
 *
 * UTC rather than the server's own zone so the fallback does not depend on how the
 * container happens to be configured; the browser re-renders in its own zone right after
 * hydrating either way (`shared/lib/time-zone.tsx`).
 */
export async function renderTimeZone(): Promise<string> {
  const value = (await cookies()).get(COOKIES.timeZone)?.value;
  return isTimeZone(value) ? value : "UTC";
}
