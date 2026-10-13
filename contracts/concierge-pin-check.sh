#!/usr/bin/env bash
# FB-24 / CROSS-5: guard the cross-repo concierge pin.
#
# Banking compiles the identity + governance wire contract from the `evconcierge_contracts`
# git dep (Cargo.toml) and re-aliases the cabinet's TS from concierge's proto. This
# asserts that pin is trustworthy:
#   1. the pinned rev is an ANCESTOR of concierge `origin/main` (not an orphaned /
#      force-pushed SHA — the supply-chain fragility the audit flagged);
#   2. the proto BYTES at the pin match what's on origin/main today (no silent drift
#      between "the proto banking compiles against" and "the proto concierge ships");
#   3. the proto BYTES in the local cargo checkout — the files `gen-api` actually feeds
#      protoc — match the pin (a stale or hand-edited checkout would otherwise generate
#      a contract nobody pinned).
#
# WHICH protos: contracts/concierge-protos.txt — the same list `gen-api` feeds protoc,
# so a proto cannot be generated without being guarded (issue #293: governance.proto).
#
# Run in CI (.github/workflows/drift.yml) and locally before bumping the pin. Needs
# network access to the concierge remote and a resolvable cargo workspace (`cargo
# metadata`, i.e. .tb-client linked — the nix app does that); exits non-zero on any
# violation so a bad pin can't merge.
set -euo pipefail

repo="$(git rev-parse --show-toplevel)"
remote="https://github.com/EV-Main/concierge.git"
list="$repo/contracts/concierge-protos.txt"

# No `mapfile`: the shebang resolves to bash 3.2 on a stock mac (issue #321).
protos=()
while IFS= read -r l; do protos+=("$l"); done < <(grep -Ev '^[[:space:]]*(#|$)' "$list")
if [ "${#protos[@]}" -eq 0 ]; then
	echo "::error::$list lists no protos" >&2
	exit 1
fi

pin="$(grep -oE 'evconcierge_contracts = \{ git = "[^"]+", (rev|tag) = "[^"]+"' "$repo/Cargo.toml" | grep -oE '(rev|tag) = "[^"]+"' | sed -E 's/.*"([^"]+)"/\1/')"
if [ -z "$pin" ]; then
	echo "::error::could not read the evconcierge_contracts pin from Cargo.toml" >&2
	exit 1
fi
echo "pinned evconcierge_contracts -> $pin"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
git -C "$work" init -q
git -C "$work" remote add origin "$remote"
# Full history, not `--depth`: a later shallow fetch (e.g. of tags) resets the shallow
# boundary and truncates origin/main, so the ancestry check below answers "no" for every
# pin (issues #371, #440). The repo is small enough to fetch whole.
git -C "$work" fetch -q --tags origin main
# Resolve the pin (rev or annotated tag) to a concrete commit, fetching the tag if needed.
git -C "$work" fetch -q origin "$pin" 2>/dev/null || true
if [ "$(git -C "$work" rev-parse --is-shallow-repository)" = "true" ]; then
	echo "::error::the concierge clone is shallow, so ancestry of the pin cannot be decided" >&2
	exit 1
fi
pin_commit="$(git -C "$work" rev-parse -q --verify "${pin}^{commit}" 2>/dev/null || git -C "$work" rev-parse -q --verify "$pin" 2>/dev/null || echo "")"
if [ -z "$pin_commit" ]; then
	echo "::error::pinned rev/tag '$pin' is not reachable from the concierge remote (orphaned or unpushed)" >&2
	exit 1
fi

if ! git -C "$work" merge-base --is-ancestor "$pin_commit" origin/main; then
	echo "::error::pinned rev $pin_commit is NOT an ancestor of concierge origin/main" >&2
	exit 1
fi
echo "ok: pin is an ancestor of origin/main"

# 2. The proto bytes banking depends on must match origin/main's.
for p in "${protos[@]}"; do
	pinned="$(git -C "$work" show "$pin_commit:contracts/proto/$p")"
	head="$(git -C "$work" show "origin/main:contracts/proto/$p")"
	if [ "$pinned" != "$head" ]; then
		echo "::error::contracts/proto/$p differs between the pin and origin/main — bump the pin deliberately" >&2
		exit 1
	fi
done
echo "ok: vendored proto bytes match origin/main (${protos[*]})"

# 3. The local cargo checkout is what `gen-api` compiles from (same `cargo metadata`
# lookup as runGenApi in flake.nix). Its bytes must be the pin's, file for file.
cc_manifest="$(cargo metadata --format-version 1 --manifest-path "$repo/contracts/Cargo.toml" \
	| jq -r '.packages[] | select(.name=="evconcierge_contracts") | .manifest_path')"
if [ -z "$cc_manifest" ] || [ ! -f "$cc_manifest" ]; then
	echo "::error::cargo metadata did not resolve the evconcierge_contracts checkout" >&2
	exit 1
fi
cc_dir="$(dirname "$cc_manifest")"
# The checkout root cargo extracted from its git db: ~/.cargo/git/checkouts/concierge-*/<sha>.
cc_root="$(dirname "$cc_dir")"
echo "local checkout -> $cc_dir"
for p in "${protos[@]}"; do
	local_file="$cc_dir/proto/$p"
	if [ ! -f "$local_file" ]; then
		echo "::error::$local_file is missing from the local concierge checkout" >&2
		exit 1
	fi
	pinned="$(git -C "$work" show "$pin_commit:contracts/proto/$p")"
	if [ "$pinned" != "$(cat "$local_file")" ]; then
		# `cargo update -p` is a no-op on a fixed rev and never re-extracts an existing
		# checkout; only removing the directory makes cargo re-extract it from its git db.
		echo "::error::$local_file differs from contracts/proto/$p at the pin — the checkout is stale or edited; remove it ('rm -rf $cc_root') and rerun: cargo re-extracts it on the next 'cargo metadata'" >&2
		exit 1
	fi
done
echo "ok: local checkout proto bytes match the pin"
