//! Engine snapshots: the whole book in a versioned, checksummed binary format.
//!
//! A snapshot and the journal of the commands that came after it reproduce the engine
//! exactly; [`crate::recovery`] puts the two together after a crash. A snapshot stores
//! every resting order in book order together with its time priority, plus the engine's
//! priority counter, so restored queues keep their order and orders entered after the
//! restore receive the same priorities as in the original session. It also stores the
//! trading phase and the auction prices, so a crash during a call phase recovers into the
//! same call.
//!
//! Layout of version 2, little-endian:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 8 | [`MAGIC`]: `XBSNAP`, a zero byte and the format version |
//! | 8 | Sequence: how many journaled commands the snapshot includes |
//! | 8 | Clock: the last time priority handed out |
//! | 8 | Number of resting orders |
//! | 1 | Phase: 0 continuous trading, 1 call phase |
//! | 1 | Flags: bit 0 a reference price is present, bit 1 a last trade price is present |
//! | 6 | Reserved, zero |
//! | 8 | Reference price of the call phase (0 when absent) |
//! | 8 | Last trade price (0 when absent) |
//! | 40 each | Resting orders in [`Engine::snapshot`] order: per side, waiting market orders, then limit orders from the best price, oldest first within a price |
//! | 8 | FNV-1a checksum of every byte before it |
//!
//! Each order record holds the id (8 bytes), account (4), side, self-trade prevention
//! mode, post-only flag and kind (0 limit, 1 market) (1 each), price (8, 0 for a market
//! order), open quantity (8) and time priority (8). [`decode`] rejects anything that is not
//! a state the engine could have produced: a bad checksum, an unknown code, a crossed book
//! in continuous trading, market or post-only orders in the wrong phase, levels or queues
//! out of order, duplicate ids or priorities, or a priority newer than the clock.
//!
//! Version 1, written before call auctions, has a 32-byte header (the first four fields)
//! and a reserved zero byte where version 2 stores the kind. [`decode`] still reads it, as
//! continuous trading with no trade price recorded; [`encode`] always writes version 2.

use core::fmt;
use std::collections::HashSet;

use crate::Engine;
use crate::digest::{Fingerprint, side_code, stp_code};
use crate::engine::SessionState;
use crate::types::{MARKET, MAX_QTY, OrderView, Phase, Price, Side, StpMode};

/// File signature; the last byte is the format version this build writes.
pub const MAGIC: [u8; 8] = *b"XBSNAP\x00\x02";

/// Header bytes before the first order record.
pub const HEADER_LEN: usize = 56;

/// Header bytes before the first order record in format version 1.
pub const HEADER_LEN_V1: usize = 32;

/// Bytes per resting order.
pub const RECORD_LEN: usize = 40;

const CHECKSUM_LEN: usize = 8;
const HAS_REFERENCE: u8 = 1;
const HAS_LAST_TRADE: u8 = 2;

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
    /// The phase or price fields of the header are invalid.
    InvalidHeader(&'static str),
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
            Self::UnsupportedVersion(version) => write!(
                f,
                "snapshot format version {version} is not supported (this build reads 1 and 2)"
            ),
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
            Self::InvalidHeader(reason) => write!(f, "invalid snapshot header: {reason}"),
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
    let state = engine.session_state();
    let mut out = Vec::with_capacity(HEADER_LEN + engine.len() * RECORD_LEN + CHECKSUM_LEN);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&state.clock.to_le_bytes());
    out.extend_from_slice(&(engine.len() as u64).to_le_bytes());
    let flags = if state.reference.is_some() {
        HAS_REFERENCE
    } else {
        0
    } | if state.last_trade.is_some() {
        HAS_LAST_TRADE
    } else {
        0
    };
    out.extend_from_slice(&[
        u8::from(state.phase == Phase::Call),
        flags,
        0,
        0,
        0,
        0,
        0,
        0,
    ]);
    out.extend_from_slice(&state.reference.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&state.last_trade.unwrap_or(0).to_le_bytes());
    engine.for_each_with_priority(|order, priority| {
        out.extend_from_slice(&order.id.to_le_bytes());
        out.extend_from_slice(&order.account.to_le_bytes());
        out.extend_from_slice(&[
            side_code(order.side),
            stp_code(order.stp),
            u8::from(order.post_only),
            u8::from(order.is_market()),
        ]);
        out.extend_from_slice(&order.price.to_le_bytes());
        out.extend_from_slice(&order.remaining.to_le_bytes());
        out.extend_from_slice(&priority.to_le_bytes());
    });
    let checksum = checksum(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    out
}

/// Restores an engine from [`encode`]'s output, or from a version 1 snapshot, and returns
/// it with the snapshot's sequence.
///
/// # Errors
/// Returns [`SnapshotError`] for anything other than an intact snapshot of a valid book.
pub fn decode(bytes: &[u8]) -> Result<(Engine, u64), SnapshotError> {
    if bytes.len() < MAGIC.len() || bytes[..MAGIC.len() - 1] != MAGIC[..MAGIC.len() - 1] {
        return Err(SnapshotError::BadMagic);
    }
    let header_len = match bytes[MAGIC.len() - 1] {
        1 => HEADER_LEN_V1,
        2 => HEADER_LEN,
        version => return Err(SnapshotError::UnsupportedVersion(version)),
    };
    if bytes.len() < header_len + CHECKSUM_LEN {
        return Err(SnapshotError::Truncated {
            needed: header_len + CHECKSUM_LEN,
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
    let count = u64_at(body, 24);
    let state = if header_len == HEADER_LEN {
        session_state(body)?
    } else {
        SessionState {
            clock: u64_at(body, 16),
            ..SessionState::default()
        }
    };
    let records = &body[header_len..];
    if count.checked_mul(RECORD_LEN as u64) != Some(records.len() as u64) {
        return Err(SnapshotError::LengthMismatch {
            orders: count,
            bytes: bytes.len(),
        });
    }

    let call = state.phase == Phase::Call;
    let mut orders = Vec::with_capacity(records.len() / RECORD_LEN);
    let mut ids = HashSet::with_capacity(orders.capacity());
    let mut priorities = HashSet::with_capacity(orders.capacity());
    // Side, market flag, price and priority of the previous record.
    let mut previous: Option<(Side, bool, Price, u64)> = None;
    let mut best_bid = None;
    let (records, _) = records.as_chunks::<RECORD_LEN>();
    for (index, record) in (0u64..).zip(records) {
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
        // Version 1 kept this byte reserved; a value of 1 there is a version 2 market order.
        let market = match record[15] {
            0 => false,
            1 if header_len == HEADER_LEN => true,
            _ => return Err(invalid("unknown order kind")),
        };
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
        if market && order.price != MARKET {
            return Err(invalid("market order with a price"));
        }
        if !market && order.price <= 0 {
            return Err(invalid("price is not positive"));
        }
        if market && !call {
            return Err(invalid("market order outside a call phase"));
        }
        if post_only && call {
            return Err(invalid("post-only order in a call phase"));
        }
        if order.remaining == 0 || order.remaining > MAX_QTY {
            return Err(invalid("open quantity is out of range"));
        }
        if priority == 0 || priority > state.clock {
            return Err(invalid("time priority is outside the clock"));
        }
        if !ids.insert(order.id) {
            return Err(invalid("duplicate order id"));
        }
        if !priorities.insert(priority) {
            return Err(invalid("duplicate time priority"));
        }
        // Each side lists its market orders by time, then its limit orders by price and time.
        match previous {
            Some((Side::Sell, ..)) if side == Side::Buy => {
                return Err(invalid("bid listed after an ask"));
            }
            Some((last_side, last_market, last_price, last_priority)) if last_side == side => {
                let in_order = match (last_market, market) {
                    (false, true) => {
                        return Err(invalid("market order listed after a limit order"));
                    }
                    (true, false) => true,
                    (true, true) => priority > last_priority,
                    (false, false) if order.price == last_price => priority > last_priority,
                    (false, false) if side == Side::Buy => order.price < last_price,
                    (false, false) => order.price > last_price,
                };
                if !in_order {
                    return Err(invalid("price levels or queue out of order"));
                }
            }
            // The first record of a side. Only a call phase may cross, and only a call phase
            // holds market orders, so in continuous trading this is the side's best price.
            _ => {
                if side == Side::Buy {
                    best_bid = Some(order.price);
                } else if !call && best_bid.is_some_and(|bid| bid >= order.price) {
                    return Err(invalid("crossed book"));
                }
            }
        }
        previous = Some((side, market, order.price, priority));
        orders.push((order, priority));
    }
    Ok((Engine::restore(state, &orders), sequence))
}

/// Reads and checks the phase and price fields of a version 2 header.
fn session_state(body: &[u8]) -> Result<SessionState, SnapshotError> {
    let invalid = SnapshotError::InvalidHeader;
    let phase = match body[32] {
        0 => Phase::Continuous,
        1 => Phase::Call,
        _ => return Err(invalid("unknown phase code")),
    };
    let flags = body[33];
    if flags & !(HAS_REFERENCE | HAS_LAST_TRADE) != 0 || body[34..40].iter().any(|&b| b != 0) {
        return Err(invalid("reserved bits are not zero"));
    }
    let price = |at: usize, flag: u8| {
        let price = i64::from_le_bytes(body[at..at + 8].try_into().expect("8-byte slice"));
        match (flags & flag != 0, price) {
            (false, 0) => Ok(None),
            (true, price) if price > 0 => Ok(Some(price)),
            _ => Err(invalid(
                "a price is not positive, or present without its flag",
            )),
        }
    };
    let reference = price(40, HAS_REFERENCE)?;
    if reference.is_some() && phase == Phase::Continuous {
        return Err(invalid("a reference price outside a call phase"));
    }
    Ok(SessionState {
        clock: u64_at(body, 16),
        phase,
        reference,
        last_trade: price(48, HAS_LAST_TRADE)?,
    })
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
        assert_eq!(restored.session_state(), engine.session_state());
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
        let truncated = |available| SnapshotError::Truncated {
            needed: HEADER_LEN + CHECKSUM_LEN,
            available,
        };
        assert_eq!(decode(&MAGIC).unwrap_err(), truncated(8));
        assert_eq!(decode(&bytes[..20]).unwrap_err(), truncated(20));
        assert_eq!(
            decode(&bytes[..HEADER_LEN + CHECKSUM_LEN - 1]).unwrap_err(),
            truncated(HEADER_LEN + CHECKSUM_LEN - 1)
        );
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
        // The best ask at or below the best bid, even if above a deeper bid, is crossed.
        for ask in [98i64, 99] {
            let mut crossed = bytes.clone();
            crossed[at(2, 16)..at(2, 24)].copy_from_slice(&ask.to_le_bytes());
            assert_eq!(
                reseal(crossed),
                SnapshotError::InvalidOrder {
                    index: 2,
                    reason: "crossed book"
                }
            );
        }
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
        for remaining in [0, MAX_QTY + 1] {
            let mut open = bytes.clone();
            open[at(1, 24)..at(1, 32)].copy_from_slice(&remaining.to_le_bytes());
            assert_eq!(
                reseal(open),
                SnapshotError::InvalidOrder {
                    index: 1,
                    reason: "open quantity is out of range"
                }
            );
        }
        let mut late_bid = bytes.clone();
        late_bid[at(3, 12)] = 0; // The last ask turns into a bid.
        assert_eq!(
            reseal(late_bid),
            SnapshotError::InvalidOrder {
                index: 3,
                reason: "bid listed after an ask"
            }
        );
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

    /// A call phase with a reference price, a last trade, a crossed book and market orders.
    fn call_sample() -> Engine {
        let mut engine = sample();
        let mut events = Vec::new();
        for command in [
            Command::New(NewOrder::limit(6, Side::Buy, 101, 2)),
            Command::StartCall {
                reference: Some(100),
            },
            Command::New(NewOrder::market(7, Side::Buy, 3).tif(crate::TimeInForce::Gtc)),
            Command::New(NewOrder::market(8, Side::Buy, 1).tif(crate::TimeInForce::Gtc)),
            Command::New(NewOrder::market(9, Side::Sell, 2).tif(crate::TimeInForce::Gtc)),
            Command::New(NewOrder::limit(10, Side::Buy, 103, 4)),
        ] {
            engine.process(&command, &mut events);
        }
        engine
    }

    #[test]
    fn a_call_phase_round_trips() {
        let engine = call_sample();
        let state = engine.session_state();
        assert_eq!(
            (state.phase, state.reference, state.last_trade),
            (Phase::Call, Some(100), Some(101))
        );
        let bytes = encode(&engine, 12);
        let (restored, _) = decode(&bytes).unwrap();
        restored.check_invariants().unwrap();
        assert_eq!(restored.snapshot(), engine.snapshot());
        assert_eq!(restored.session_state(), state);
        assert_eq!(restored.indicative_uncross(), engine.indicative_uncross());
        assert_eq!(encode(&restored, 12), bytes);
        let market: Vec<_> = engine.snapshot().iter().map(OrderView::is_market).collect();
        assert_eq!(market, [true, true, false, false, true, false, false]);
    }

    #[test]
    fn rejects_invalid_phase_state() {
        let bytes = encode(&call_sample(), 12);
        let reseal = |mut body: Vec<u8>| {
            body.truncate(body.len() - CHECKSUM_LEN);
            let sum = checksum(&body);
            body.extend_from_slice(&sum.to_le_bytes());
            decode(&body).unwrap_err()
        };
        let header = |at: usize, value: u8| {
            let mut patched = bytes.clone();
            patched[at] = value;
            reseal(patched)
        };
        let header_error = |reason| SnapshotError::InvalidHeader(reason);
        assert_eq!(header(32, 2), header_error("unknown phase code"));
        assert_eq!(header(33, 4), header_error("reserved bits are not zero"));
        assert_eq!(header(39, 1), header_error("reserved bits are not zero"));
        let price_error = header_error("a price is not positive, or present without its flag");
        assert_eq!(header(33, HAS_LAST_TRADE), price_error);
        assert_eq!(header(33, HAS_REFERENCE), price_error);
        for price in [-1i64, 0] {
            let mut flagged = bytes.clone();
            flagged[48..56].copy_from_slice(&price.to_le_bytes());
            assert_eq!(reseal(flagged), price_error);
        }
        // Continuous trading: the reference price and the market orders are out of place.
        assert_eq!(
            header(32, 0),
            header_error("a reference price outside a call phase")
        );
        let mut continuous = bytes.clone();
        continuous[32] = 0;
        continuous[33] = HAS_LAST_TRADE;
        continuous[40..48].fill(0);
        assert_eq!(
            reseal(continuous),
            SnapshotError::InvalidOrder {
                index: 0,
                reason: "market order outside a call phase"
            }
        );

        let at = |index: usize, field: usize| HEADER_LEN + index * RECORD_LEN + field;
        let record = |index: usize, field: usize, value: u8| {
            let mut patched = bytes.clone();
            patched[at(index, field)] = value;
            reseal(patched)
        };
        let order_error = |index, reason| SnapshotError::InvalidOrder { index, reason };
        assert_eq!(record(0, 15, 2), order_error(0, "unknown order kind"));
        assert_eq!(
            record(0, 16, 1),
            order_error(0, "market order with a price")
        );
        assert_eq!(
            record(2, 15, 1),
            order_error(2, "market order with a price")
        );
        assert_eq!(
            record(2, 14, 1),
            order_error(2, "post-only order in a call phase")
        );
        assert_eq!(record(4, 15, 0), order_error(4, "price is not positive"));
        // The two market buys out of time order.
        let mut swapped = bytes.clone();
        swapped[at(0, 32)..at(0, 40)].copy_from_slice(&7u64.to_le_bytes());
        swapped[at(1, 32)..at(1, 40)].copy_from_slice(&6u64.to_le_bytes());
        assert_eq!(
            reseal(swapped),
            order_error(1, "price levels or queue out of order")
        );
        // A limit bid listed before the market buys.
        let mut late = bytes.clone();
        let (first, third) = (
            bytes[at(0, 0)..at(1, 0)].to_vec(),
            bytes[at(2, 0)..at(3, 0)].to_vec(),
        );
        late[at(0, 0)..at(1, 0)].copy_from_slice(&third);
        late[at(2, 0)..at(3, 0)].copy_from_slice(&first);
        assert_eq!(
            reseal(late),
            order_error(1, "market order listed after a limit order")
        );
    }

    #[test]
    fn version_1_snapshots_still_load() {
        // Written by crossbook 0.2: `crossbook snapshot` of seed 5 at 200 commands.
        let v1 = include_bytes!("../tests/fixtures/v1-snapshot.bin");
        assert_eq!(&v1[..8], b"XBSNAP\x00\x01");
        let (engine, sequence) = decode(v1).unwrap();
        engine.check_invariants().unwrap();
        assert_eq!(sequence, 200);
        assert_eq!(engine.len(), 42);
        assert_eq!(format!("{:016x}", engine.state_hash()), "0fa465ad40064a62");
        let state = engine.session_state();
        assert_eq!(
            (state.phase, state.reference, state.last_trade),
            (Phase::Continuous, None, None)
        );
        // Version 1 has no kind byte: a 1 in that reserved position is invalid there.
        let mut patched = v1.to_vec();
        patched[HEADER_LEN_V1 + 15] = 1;
        patched.truncate(patched.len() - CHECKSUM_LEN);
        let sum = checksum(&patched);
        patched.extend_from_slice(&sum.to_le_bytes());
        assert_eq!(
            decode(&patched).unwrap_err(),
            SnapshotError::InvalidOrder {
                index: 0,
                reason: "unknown order kind"
            }
        );
        assert_eq!(
            decode(&v1[..HEADER_LEN_V1 + CHECKSUM_LEN - 1]).unwrap_err(),
            SnapshotError::Truncated {
                needed: HEADER_LEN_V1 + CHECKSUM_LEN,
                available: HEADER_LEN_V1 + CHECKSUM_LEN - 1
            }
        );
        assert!(
            SnapshotError::UnsupportedVersion(3)
                .to_string()
                .contains("reads 1 and 2")
        );
    }
}
