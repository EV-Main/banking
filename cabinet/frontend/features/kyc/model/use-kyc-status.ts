"use client";

// One answer to "what may this screen offer?", so no screen has to compose a tier and a case
// for itself.
//
// Its callers: the home card's start row and status chip, and the money screens'
// `VerificationRequired` block (#215). Offering a doomed start there costs a paid vendor
// session, which is why each reads this and not the profile's mirrored tier alone.
//
// Two sources, in a fixed order of trust. `/kyc/status` is authoritative and fresh — it is
// the only thing that knows a case is running. The profile is the fallback, and stays the
// answer when the plane cannot be read at all: a cabinet deployed ahead of the concierge
// release that adds the route must behave exactly as it does today, offering a start on tier
// 0, rather than going blank or refusing one.

import { useEffect } from "react";

import type { KycCase } from "@/features/kyc/api/kyc-contract";
import { useKycCompletedSignal } from "@/features/kyc/model/kyc-signals";
import { kycStatusResource } from "@/features/kyc/model/kyc-status-resource";
import { canStartVerification, kycLevel } from "@/entities/user/lib/kyc";
import { profileResource } from "@/entities/user/model/profile-resource";
import { useResource } from "@/shared/lib/resource";

export interface KycGate {
  /** The tier to believe: the plane's when it answered, the profile's otherwise. */
  level: number;
  /** The attempt running right now. `null` when none is — and when nothing could be read. */
  runningCase: KycCase | null;
  /** Whether a start may be offered. See `canStartVerification`. */
  canStart: boolean;
  /** `false` means the plane did not answer and every field above came from the profile. */
  known: boolean;
  /** Nothing has been read yet from either source — a screen shows a skeleton, not a state. */
  loading: boolean;
}

export function useKycStatus(): KycGate {
  const { data: status, isLoading: statusLoading } = useResource(kycStatusResource);
  const { data: profile, isLoading: profileLoading } = useResource(profileResource);

  const known = status != null;
  const level = known ? status.level : kycLevel(profile);
  const runningCase = known ? status.case : null;

  // The tier the profile carries is mirrored across the bridge and arrives a poll behind the
  // plane's own answer, while the profile is what the account chip, the level pill and the
  // money screens read. So the moment the two disagree, ask for the profile again rather than
  // leaving the page holding two different tiers at once.
  //
  // This is a shortcut, not the guarantee: it fires once per disagreement, and the mirror may
  // still be behind when the answer comes back. `profile-resource`'s own poll is what closes
  // the gap for certain; this just makes the common case immediate.
  const profileLevel = profile == null ? null : kycLevel(profile);
  const planeLevel = known ? status.level : null;
  useEffect(() => {
    if (planeLevel !== null && profileLevel !== null && planeLevel !== profileLevel) profileResource.invalidate();
  }, [planeLevel, profileLevel]);

  const loading = (statusLoading && status === undefined) || (profileLoading && profile === undefined);
  useKycCompletedSignal(level, loading);

  return {
    level,
    runningCase,
    canStart: canStartVerification(level, runningCase),
    known,
    loading,
  };
}
