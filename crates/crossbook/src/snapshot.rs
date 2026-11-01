//! Engine snapshots: the whole book in a versioned, checksummed binary format.
//!
//! A snapshot and the journal of the commands that came after it reproduce the engine
//! exactly; [`crate::recovery`] puts the two together after a crash. A snapshot stores
//! every resting order in book order together with its time priority, plus the engine's
//! priority counter, so restored queues keep their order and orders entered after the
//! restore receive the same priorities as in the original session.
//!
//! Layout, little-endian:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 8 | [`MAGIC`]: `XBSNAP`, a zero byte and the format version |
//! | 8 | Sequence: how many journaled commands the snapshot includes |
//! | 8 | Clock: the last time priority handed out |
//! | 8 | Number of resting orders |
//! | 40 each | Resting orders: bids from the best price down, then asks from the best price up, oldest first within a price |
//! | 8 | FNV-1a checksum of every byte before it |
//!
//! Each order record holds the id (8 bytes), account (4), side, self-trade prevention
//! mode, post-only flag and a reserved zero byte (1 each), price (8), open quantity (8)
//! and time priority (8). [`decode`] rejects anything that is not a book the engine
//! could have produced: a bad checksum, an unknown code, a crossed book, levels or
//! queues out of order, duplicate ids or priorities, or a priority newer than the clock.

use core::fmt;
use std::collections::HashSet;

use crate::Engine;
use crate::digest::{Fingerprint, side_code, stp_code};
use crate::types::{MAX_QTY, OrderView, Price, Side, StpMode};

/// File signature; the last byte is the format version.
pub const MAGIC: [u8; 8] = *b"XBSNAP\x00\x01";

/// Header bytes before the first order record.
pub const HEADER_LEN: usize = 32;

/// Bytes per resting order.
pub const RECORD_LEN: usize = 40;

const CHECKSUM_LEN: usize = 8;

/// Why bytes are not a usable snapshot. Decoding never panics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// The data does not start with the snapshot signature.
    BadMagic,
    /// The data is a snapshot in a format version this build does not read.
    UnsupportedVersion(u8),
    /// The data ends before the header and checksum do.
    Truncated {
        /// Bytes the header and checksum need.
        needed: usize,
        /// Bytes available.
        available: usize,
    },
    /// The stored checksum does not match the data, so the snapshot is damaged.
    ChecksumMismatch {
        /// Checksum stored in the snapshot.
        stored: u64,
        /// Checksum of the data as read.
        computed: u64,
    },
    /// The order count does not fit the data length.
    LengthMismatch {
        /// Orders the header declares.
        orders: u64,
        /// Bytes in the snapshot.
        bytes: usize,
    },
    /// A resting order is invalid or out of book order.
    InvalidOrder {
        /// Position of the record, from 0.
        index: u64,
        /// The first check that failed.
        reason: &'static str,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::BadMagic => f.write_str("not a crossbook snapshot (bad header)"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported snapshot format version {version}")
            }
            Self::Truncated { needed, available } => {
                write!(
                    f,
                    "truncated snapshot: need {needed} bytes, have {available}"
                )
            }
            Self::ChecksumMismatch { stored, computed } => write!(
                f,
                "snapshot checksum mismatch: stored {stored:016x}, computed {computed:016x}"
            ),
            Self::LengthMismatch { orders, bytes } => {
                write!(
                    f,
                    "snapshot declares {orders} orders but holds {bytes} bytes"
                )
            }
            Self::InvalidOrder { index, reason } => {
                write!(f, "invalid resting order {index} in snapshot: {reason}")
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Serializes `engine`. `sequence` is the number of journaled commands the book includes;
/// recovery skips that many commands before replaying the rest of the journal.
#[must_use]
pub fn encode(engine: &Engine, sequence: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + engine.len() * RECORD_LEN + CHECKSUM_LEN);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&engine.clock().to_le_bytes());
    out.extend_from_slice(&(engine.len() as u64).to_le_bytes());
    engine.for_each_with_priority(|order, priority| {
        out.extend_from_slice(&order.id.to_le_bytes());
        out.extend_from_slice(&order.account.to_le_bytes());
        out.extend_from_slice(&[
            side_code(order.side),
            stp_code(order.stp),
            u8::from(order.post_only),
            0,
        ]);
        out.extend_from_slice(&order.price.to_le_bytes());
        out.extend_from_slice(&order.remaining.to_le_bytes());
        out.extend_from_slice(&priority.to_le_bytes());
    });
    let checksum = checksum(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    out
}

/// Restores an engine from [`encode`]'s output and returns it with the snapshot's sequence.
///
/// # Errors
/// Returns [`SnapshotError`] for anything other than an intact snapshot of a valid book.
pub fn decode(bytes: &[u8]) -> Result<(Engine, u64), SnapshotError> {
    if bytes.len() < MAGIC.len() || bytes[..MAGIC.len() - 1] != MAGIC[..MAGIC.len() - 1] {
        return Err(SnapshotError::BadMagic);
    }
    let version = bytes[MAGIC.len() - 1];
    if version != MAGIC[MAGIC.len() - 1] {
        return Err(SnapshotError::UnsupportedVersion(version));
    }
    if bytes.len() < HEADER_LEN + CHECKSUM_LEN {
        return Err(SnapshotError::Truncated {
            needed: HEADER_LEN + CHECKSUM_LEN,
            available: bytes.len(),
        });
    }
    let (body, stored) = bytes.split_at(bytes.len() - CHECKSUM_LEN);
    let stored = u64_at(stored, 0);
    let computed = checksum(body);
    if stored != computed {
        return Err(SnapshotError::ChecksumMismatch { stored, computed });
    }

    let sequence = u64_at(body, 8);
    let clock = u64_at(body, 16);
    let count = u64_at(body, 24);
    let records = &body[HEADER_LEN..];
    if count.checked_mul(RECORD_LEN as u64) != Some(records.len() as u64) {
        return Err(SnapshotError::LengthMismatch {
            orders: count,
            bytes: bytes.len(),
        });
    }

    let mut orders = Vec::with_capacity(records.len() / RECORD_LEN);
    let mut ids = HashSet::with_capacity(orders.capacity());
    let mut priorities = HashSet::with_capacity(orders.capacity());
    let mut previous: Option<(Side, Price, u64)> = None;
    let mut best_bid = None;
    for (index, record) in (0u64..).zip(records.chunks_exact(RECORD_LEN)) {
        let invalid = |reason| SnapshotError::InvalidOrder { index, reason };
        let side = match record[12] {
            0 => Side::Buy,
            1 => Side::Sell,
            _ => return Err(invalid("unknown side code")),
        };
        let stp = match record[13] {
            0 => StpMode::Allow,
            1 => StpMode::CancelNewest,
            2 => StpMode::CancelOldest,
            3 => StpMode::CancelBoth,
            _ => return Err(invalid("unknown self-trade prevention code")),
        };
        let post_only = match record[14] {
            0 => false,
            1 => true,
            _ => return Err(invalid("post-only flag is not 0 or 1")),
        };
        if record[15] != 0 {
            return Err(invalid("reserved byte is not zero"));
        }
        let order = OrderView {
            id: u64_at(record, 0),
            account: u32::from_le_bytes(record[8..12].try_into().expect("4-byte slice")),
            side,
            price: i64::from_le_bytes(record[16..24].try_into().expect("8-byte slice")),
            remaining: u64_at(record, 24),
            stp,
            post_only,
        };
        let priority = u64_at(record, 32);
        if order.price <= 0 {
            return Err(invalid("price is not positive"));
        }
        if order.remaining == 0 || order.remaining > MAX_QTY {
            return Err(invalid("open quantity is out of range"));
        }
        if priority == 0 || priority > clock {
            return Err(invalid("time priority is outside the clock"));
        }
        if !ids.insert(order.id) {
            return Err(invalid("duplicate order id"));
        }
        if !priorities.insert(priority) {
            return Err(invalid("duplicate time priority"));
        }
        match previous {
            Some((Side::Sell, _, _)) if side == Side::Buy => {
                return Err(invalid("bid listed after an ask"));
            }
            Some((last_side, last_price, last_priority)) if last_side == side => {
                let in_order = if order.price == last_price {
                    priority > last_priority
                } else if side == Side::Buy {
                    order.price < last_price
                } else {
                    order.price > last_price
                };
                if !in_order {
                    return Err(invalid("price levels or queue out of order"));
                }
            }
            _ => {
                if side == Side::Buy {
                    best_bid = Some(order.price);
                } else if best_bid.is_some_and(|bid| bid >= order.price) {
                    return Err(invalid("crossed book"));
                }
            }
        }
        previous = Some((side, order.price, priority));
        orders.push((order, priority));
    }
    Ok((Engine::restore(clock, &orders), sequence))
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut fingerprint = Fingerprint::new();
    fingerprint.bytes(bytes);
    fingerprint.value()
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8-byte slice"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Command, NewOrder};

    fn sample() -> Engine {
        let mut engine = Engine::new();
        let mut events = Vec::new();
        for command in [
            Command::New(NewOrder::limit(1, Side::Buy, 99, 5).account(7)),
            Command::New(NewOrder::limit(2, Side::Buy, 99, 3).post_only()),
            Command::New(NewOrder::limit(3, Side::Buy, 98, 4).stp(StpMode::CancelBoth)),
            Command::New(NewOrder::limit(4, Side::Sell, 101, 6)),
            Command::New(NewOrder::limit(5, Side::Sell, 102, 2)),
            Command::Cancel { id: 1 },
        ] {
            engine.process(&command, &mut events);
        }
        engine
    }

    #[test]
    fn round_trip_keeps_orders_priorities_and_clock() {
        let engine = sample();
        let bytes = encode(&engine, 6);
        assert_eq!(bytes.len(), HEADER_LEN + 4 * RECORD_LEN + CHECKSUM_LEN);
        let (restored, sequence) = decode(&bytes).unwrap();
        assert_eq!(sequence, 6);
        assert_eq!(restored.snapshot(), engine.snapshot());
        assert_eq!(restored.clock(), engine.clock());
        restored.check_invariants().unwrap();
        assert_eq!(encode(&restored, 6), bytes, "re-encoding is byte-identical");
        assert_eq!(decode(&encode(&Engine::new(), 0)).unwrap().0.len(), 0);
    }

    #[test]
    fn rejects_damage_and_invalid_books() {
        let bytes = encode(&sample(), 6);
        assert_eq!(decode(&bytes[..5]).unwrap_err(), SnapshotError::BadMagic);
        let mut version = bytes.clone();
        version[7] = 9;
        assert_eq!(
            decode(&version).unwrap_err(),
            SnapshotError::UnsupportedVersion(9)
        );
        assert!(matches!(
            decode(&bytes[..20]),
            Err(SnapshotError::Truncated { .. })
        ));
        for position in [9, HEADER_LEN + 3, bytes.len() - 1] {
            let mut flipped = bytes.clone();
            flipped[position] ^= 0x40;
            assert!(matches!(
                decode(&flipped),
                Err(SnapshotError::ChecksumMismatch { .. })
            ));
        }

        // Valid checksums over invalid books: patch a record, then re-seal.
        let reseal = |mut body: Vec<u8>| {
            body.truncate(body.len() - CHECKSUM_LEN);
            let sum = checksum(&body);
            body.extend_from_slice(&sum.to_le_bytes());
            decode(&body).unwrap_err()
        };
        let at = |index: usize, field: usize| HEADER_LEN + index * RECORD_LEN + field;
        let mut crossed = bytes.clone();
        crossed[at(2, 16)..at(2, 24)].copy_from_slice(&98i64.to_le_bytes());
        assert_eq!(
            reseal(crossed),
            SnapshotError::InvalidOrder {
                index: 2,
                reason: "crossed book"
            }
        );
        let out_of_order = SnapshotError::InvalidOrder {
            index: 1,
            reason: "price levels or queue out of order",
        };
        // The second bid level priced above the first.
        let mut levels = bytes.clone();
        levels[at(1, 16)..at(1, 24)].copy_from_slice(&100i64.to_le_bytes());
        assert_eq!(reseal(levels), out_of_order);
        // The second bid moved into the first level with an older priority.
        let mut queue = bytes.clone();
        queue[at(1, 16)..at(1, 24)].copy_from_slice(&99i64.to_le_bytes());
        queue[at(1, 32)..at(1, 40)].copy_from_slice(&1u64.to_le_bytes());
        assert_eq!(reseal(queue), out_of_order);
        let mut future = bytes.clone();
        future[at(3, 32)..at(3, 40)].copy_from_slice(&999u64.to_le_bytes());
        assert!(matches!(
            reseal(future),
            SnapshotError::InvalidOrder { index: 3, .. }
        ));
        let mut count = bytes;
        count[24] = 5;
        assert!(matches!(
            reseal(count),
            SnapshotError::LengthMismatch { orders: 5, .. }
        ));
    }
}
