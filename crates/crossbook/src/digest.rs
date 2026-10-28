//! Stable 64-bit fingerprints of event streams and book state.
//!
//! The engine is deterministic: the same commands always produce the same events and the
//! same final book. These fingerprints make that checkable across runs, machines and
//! targets (the WebAssembly playground reports the same digests as the native CLI).
//! They use FNV-1a over a fixed little-endian encoding, so they do not depend on
//! `std::hash`, whose output may change between Rust releases.

use crate::types::{CancelReason, Event, OrderView, Price, RejectReason, Side, StpMode};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Incremental FNV-1a (64-bit) hasher with a stable byte encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint(u64);

impl Default for Fingerprint {
    fn default() -> Self {
        Self::new()
    }
}

impl Fingerprint {
    /// Empty fingerprint.
    #[must_use]
    pub const fn new() -> Self {
        Self(FNV_OFFSET)
    }

    /// Current value.
    #[must_use]
    pub const fn value(&self) -> u64 {
        self.0
    }

    /// Mixes raw bytes.
    pub fn bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    /// Mixes one byte.
    pub fn u8(&mut self, value: u8) {
        self.bytes(&[value]);
    }

    /// Mixes a `u32` as little-endian bytes.
    pub fn u32(&mut self, value: u32) {
        self.bytes(&value.to_le_bytes());
    }

    /// Mixes a `u64` as little-endian bytes.
    pub fn u64(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }

    /// Mixes an `i64` as little-endian bytes.
    pub fn i64(&mut self, value: i64) {
        self.bytes(&value.to_le_bytes());
    }

    /// Mixes one event using its canonical encoding.
    pub fn event(&mut self, event: &Event) {
        match *event {
            Event::Accepted {
                id,
                side,
                price,
                qty,
            } => {
                self.u8(1);
                self.u64(id);
                self.u8(side_code(side));
                self.optional_price(price);
                self.u64(qty);
            }
            Event::Rejected { id, reason } => {
                self.u8(2);
                self.u64(id);
                self.u8(reject_code(reason));
            }
            Event::Trade {
                taker,
                maker,
                taker_side,
                price,
                qty,
                taker_remaining,
                maker_remaining,
            } => {
                self.u8(3);
                self.u64(taker);
                self.u64(maker);
                self.u8(side_code(taker_side));
                self.i64(price);
                self.u64(qty);
                self.u64(taker_remaining);
                self.u64(maker_remaining);
            }
            Event::Booked {
                id,
                side,
                price,
                qty,
            } => {
                self.u8(4);
                self.u64(id);
                self.u8(side_code(side));
                self.i64(price);
                self.u64(qty);
            }
            Event::Canceled {
                id,
                remaining,
                reason,
            } => {
                self.u8(5);
                self.u64(id);
                self.u64(remaining);
                self.u8(cancel_code(reason));
            }
            Event::Modified {
                id,
                price,
                qty,
                kept_priority,
            } => {
                self.u8(6);
                self.u64(id);
                self.i64(price);
                self.u64(qty);
                self.u8(u8::from(kept_priority));
            }
        }
    }

    /// Mixes one resting order using its canonical encoding.
    pub fn order(&mut self, order: &OrderView) {
        self.u64(order.id);
        self.u32(order.account);
        self.u8(side_code(order.side));
        self.i64(order.price);
        self.u64(order.remaining);
        self.u8(stp_code(order.stp));
        self.u8(u8::from(order.post_only));
    }

    fn optional_price(&mut self, price: Option<Price>) {
        match price {
            Some(price) => {
                self.u8(1);
                self.i64(price);
            }
            None => self.u8(0),
        }
    }
}

/// Wire code for a side (shared with the codec).
pub(crate) const fn side_code(side: Side) -> u8 {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

/// Wire code for a self-trade prevention mode (shared with the codec).
pub(crate) const fn stp_code(stp: StpMode) -> u8 {
    match stp {
        StpMode::Allow => 0,
        StpMode::CancelNewest => 1,
        StpMode::CancelOldest => 2,
        StpMode::CancelBoth => 3,
    }
}

const fn reject_code(reason: RejectReason) -> u8 {
    match reason {
        RejectReason::InvalidQuantity => 1,
        RejectReason::InvalidPrice => 2,
        RejectReason::MarketOrderCannotRest => 3,
        RejectReason::PostOnlyRequiresGtcLimit => 4,
        RejectReason::DuplicateOrderId => 5,
        RejectReason::UnknownOrder => 6,
        RejectReason::PostOnlyWouldCross => 7,
        RejectReason::FillOrKillUnfillable => 8,
    }
}

const fn cancel_code(reason: CancelReason) -> u8 {
    match reason {
        CancelReason::Requested => 1,
        CancelReason::Unfilled => 2,
        CancelReason::SelfTrade => 3,
    }
}

/// Fingerprint of a whole event sequence.
#[must_use]
pub fn events(events: &[Event]) -> u64 {
    let mut fingerprint = Fingerprint::new();
    for event in events {
        fingerprint.event(event);
    }
    fingerprint.value()
}

/// Fingerprint of a book snapshot in canonical order (see [`crate::Engine::snapshot`]).
#[must_use]
pub fn book(orders: &[OrderView]) -> u64 {
    let mut fingerprint = Fingerprint::new();
    for order in orders {
        fingerprint.order(order);
    }
    fingerprint.value()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_matches_published_vectors() {
        // Reference values from the FNV specification test suite.
        let mut empty = Fingerprint::new();
        empty.bytes(b"");
        assert_eq!(empty.value(), 0xcbf2_9ce4_8422_2325);
        let mut a = Fingerprint::new();
        a.bytes(b"a");
        assert_eq!(a.value(), 0xaf63_dc4c_8601_ec8c);
        let mut foobar = Fingerprint::new();
        foobar.bytes(b"foobar");
        assert_eq!(foobar.value(), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn different_events_give_different_fingerprints() {
        let booked = Event::Booked {
            id: 1,
            side: Side::Buy,
            price: 100,
            qty: 5,
        };
        let booked_sell = Event::Booked {
            id: 1,
            side: Side::Sell,
            price: 100,
            qty: 5,
        };
        assert_ne!(events(&[booked]), events(&[booked_sell]));
        assert_ne!(
            events(&[booked, booked_sell]),
            events(&[booked_sell, booked])
        );
    }
}
