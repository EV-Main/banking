// Server-only. `localised-status` imports `messagesFor`, which statically imports
// every catalogue — a Client Component must not reach this barrel. The one client
// status surface (the 500) reads its copy through the `I18nProvider` already
// mounted in the root layout; see `app/[locale]/cabinet/error.tsx`.
export { LocalisedStatus, type StatusKind } from "./ui/localised-status";
