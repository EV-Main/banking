"use client";

import { useEffect } from "react";

import { rememberTimeZone } from "@/shared/lib/time-zone";

// Stores the device's time zone for the next server render, so the times it puts in the
// HTML are the ones this browser would have shown. Renders nothing; mounts once, in the
// `(host)` layout. A first visit (or a device that changed zone) renders in the old zone and
// corrects itself after hydration — `useTimeZone` handles that hand-over.
export function TimeZoneSync() {
  useEffect(rememberTimeZone, []);
  return null;
}
