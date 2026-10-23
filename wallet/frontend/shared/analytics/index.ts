// Product analytics glue that `@evinvest/analytics` does not carry yet: the funnel's event
// names, the "once"/"first" idempotency marks, and the one vendor call (`identify`).
export { ACTIVATION, type ActivationEvent } from "@/shared/analytics/events";
export { createIdentifier, identity, type IdentifyClient, type Identifier } from "@/shared/analytics/identify";
export { mark, marked, once, unmark, type MarkScope } from "@/shared/analytics/marks";
