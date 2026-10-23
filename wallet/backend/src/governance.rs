//! The ownership plane's removal vote, as the emailed approval surface casts it.
//!
//! The RPCs themselves live with every other plane's in [`crate::state`], and the wire
//! conversions in [`crate::dto`]. What is left here is what belongs to neither: the words
//! a vote may land on.

use evconcierge_contracts::concierge::v1 as cc;

/// What a peer owner may answer on a REMOVAL. `Remove` carries the proposal; a single
/// `Keep` ends the peer-unanimity path.
#[derive(Clone, Copy)]
pub enum RemovalVote {
	Remove,
	Keep,
}

impl RemovalVote {
	pub fn wire(self) -> cc::RemovalVote {
		match self {
			Self::Remove => cc::RemovalVote::Remove,
			Self::Keep => cc::RemovalVote::Keep,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The vote must land on the proto variant it names, and not on the plane's
	/// `UNSPECIFIED`/`PENDING` — either of which a server could read as "no answer yet"
	/// while the owner believes they voted.
	#[test]
	fn every_vote_maps_onto_its_own_proto_variant() {
		assert_eq!(RemovalVote::Remove.wire(), cc::RemovalVote::Remove);
		assert_eq!(RemovalVote::Keep.wire(), cc::RemovalVote::Keep);
	}
}
