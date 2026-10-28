//! Fixed-layout binary encoding for commands.
//!
//! Every message is a fixed size chosen by its first byte, with little-endian integers
//! and zeroed reserved bytes, so a decoder never allocates and never guesses:
//!
//! | type | message  | bytes | layout                                                                 |
//! |------|----------|-------|------------------------------------------------------------------------|
//! | 1    | `New`    | 40    | type, side, kind, tif, flags, stp, 2 reserved, id u64, account u32, 4 reserved, price i64, qty u64 |
//! | 2    | `Cancel` | 16    | type, 7 reserved, id u64                                               |
//! | 3    | `Modify` | 32    | type, 7 reserved, id u64, price i64, qty u64                           |
//!
//! Field codes: side 0 buy / 1 sell; kind 0 limit / 1 market; tif 0 GTC / 1 IOC / 2 FOK;
//! flags bit 0 post-only; stp 0 allow / 1 cancel newest / 2 cancel oldest / 3 cancel both.
//! A market order carries price 0. The decoder checks syntax only; the engine still
//! validates quantities, prices and book state.

use core::fmt;

use crate::digest::{side_code, stp_code};
use crate::types::{Command, NewOrder, OrderKind, Side, StpMode, TimeInForce};

/// Encoded size of a `New` message.
pub const NEW_LEN: usize = 40;
/// Encoded size of a `Cancel` message.
pub const CANCEL_LEN: usize = 16;
/// Encoded size of a `Modify` message.
pub const MODIFY_LEN: usize = 32;

const TYPE_NEW: u8 = 1;
const TYPE_CANCEL: u8 = 2;
const TYPE_MODIFY: u8 = 3;

/// Why bytes could not be decoded into a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The buffer ends before the message does.
    Truncated {
        /// Bytes the message needs.
        needed: usize,
        /// Bytes available.
        available: usize,
    },
    /// The first byte is not a known message type.
    UnknownType(u8),
    /// A field holds a value outside its code table.
    InvalidField {
        /// Field name.
        field: &'static str,
        /// Raw value.
        value: u8,
    },
    /// A reserved byte is not zero.
    NonZeroReserved,
    /// A market order carries a non-zero price.
    MarketPrice,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Truncated { needed, available } => {
                write!(
                    f,
                    "truncated message: need {needed} bytes, have {available}"
                )
            }
            Self::UnknownType(code) => write!(f, "unknown message type {code}"),
            Self::InvalidField { field, value } => write!(f, "invalid {field} code {value}"),
            Self::NonZeroReserved => f.write_str("reserved byte is not zero"),
            Self::MarketPrice => f.write_str("market order with a non-zero price"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Encoded size of `command`.
#[must_use]
pub const fn encoded_len(command: &Command) -> usize {
    match command {
        Command::New(_) => NEW_LEN,
        Command::Cancel { .. } => CANCEL_LEN,
        Command::Modify { .. } => MODIFY_LEN,
    }
}

/// Appends the encoding of `command` to `buf`.
pub fn encode(command: &Command, buf: &mut Vec<u8>) {
    match *command {
        Command::New(order) => {
            let (kind, price) = match order.kind {
                OrderKind::Limit(price) => (0u8, price),
                OrderKind::Market => (1u8, 0),
            };
            let tif = match order.tif {
                TimeInForce::Gtc => 0u8,
                TimeInForce::Ioc => 1,
                TimeInForce::Fok => 2,
            };
            buf.extend_from_slice(&[
                TYPE_NEW,
                side_code(order.side),
                kind,
                tif,
                u8::from(order.post_only),
                stp_code(order.stp),
                0,
                0,
            ]);
            buf.extend_from_slice(&order.id.to_le_bytes());
            buf.extend_from_slice(&order.account.to_le_bytes());
            buf.extend_from_slice(&[0; 4]);
            buf.extend_from_slice(&price.to_le_bytes());
            buf.extend_from_slice(&order.qty.to_le_bytes());
        }
        Command::Cancel { id } => {
            buf.extend_from_slice(&[TYPE_CANCEL, 0, 0, 0, 0, 0, 0, 0]);
            buf.extend_from_slice(&id.to_le_bytes());
        }
        Command::Modify { id, price, qty } => {
            buf.extend_from_slice(&[TYPE_MODIFY, 0, 0, 0, 0, 0, 0, 0]);
            buf.extend_from_slice(&id.to_le_bytes());
            buf.extend_from_slice(&price.to_le_bytes());
            buf.extend_from_slice(&qty.to_le_bytes());
        }
    }
}

/// Decodes one command from the front of `buf` and returns it with its encoded length.
///
/// # Errors
/// Returns [`DecodeError`] for a short buffer, an unknown type, an out-of-range code, a
/// non-zero reserved byte, or a market order with a price.
pub fn decode(buf: &[u8]) -> Result<(Command, usize), DecodeError> {
    let Some(&kind) = buf.first() else {
        return Err(DecodeError::Truncated {
            needed: 1,
            available: 0,
        });
    };
    let needed = match kind {
        TYPE_NEW => NEW_LEN,
        TYPE_CANCEL => CANCEL_LEN,
        TYPE_MODIFY => MODIFY_LEN,
        other => return Err(DecodeError::UnknownType(other)),
    };
    if buf.len() < needed {
        return Err(DecodeError::Truncated {
            needed,
            available: buf.len(),
        });
    }
    let frame = &buf[..needed];
    let command = match kind {
        TYPE_NEW => decode_new(frame)?,
        TYPE_CANCEL => {
            reserved(&frame[1..8])?;
            Command::Cancel {
                id: u64_at(frame, 8),
            }
        }
        _ => {
            reserved(&frame[1..8])?;
            Command::Modify {
                id: u64_at(frame, 8),
                price: i64_at(frame, 16),
                qty: u64_at(frame, 24),
            }
        }
    };
    Ok((command, needed))
}

fn decode_new(frame: &[u8]) -> Result<Command, DecodeError> {
    let side = match frame[1] {
        0 => Side::Buy,
        1 => Side::Sell,
        value => {
            return Err(DecodeError::InvalidField {
                field: "side",
                value,
            });
        }
    };
    let tif = match frame[3] {
        0 => TimeInForce::Gtc,
        1 => TimeInForce::Ioc,
        2 => TimeInForce::Fok,
        value => {
            return Err(DecodeError::InvalidField {
                field: "tif",
                value,
            });
        }
    };
    let post_only = match frame[4] {
        0 => false,
        1 => true,
        value => {
            return Err(DecodeError::InvalidField {
                field: "flags",
                value,
            });
        }
    };
    let stp = match frame[5] {
        0 => StpMode::Allow,
        1 => StpMode::CancelNewest,
        2 => StpMode::CancelOldest,
        3 => StpMode::CancelBoth,
        value => {
            return Err(DecodeError::InvalidField {
                field: "stp",
                value,
            });
        }
    };
    reserved(&frame[6..8])?;
    reserved(&frame[20..24])?;
    let price = i64_at(frame, 24);
    let kind = match frame[2] {
        0 => OrderKind::Limit(price),
        1 if price == 0 => OrderKind::Market,
        1 => return Err(DecodeError::MarketPrice),
        value => {
            return Err(DecodeError::InvalidField {
                field: "kind",
                value,
            });
        }
    };
    Ok(Command::New(NewOrder {
        id: u64_at(frame, 8),
        account: u32::from_le_bytes(frame[16..20].try_into().expect("4-byte slice")),
        side,
        kind,
        qty: u64_at(frame, 32),
        tif,
        post_only,
        stp,
    }))
}

fn reserved(bytes: &[u8]) -> Result<(), DecodeError> {
    if bytes.iter().all(|&byte| byte == 0) {
        Ok(())
    } else {
        Err(DecodeError::NonZeroReserved)
    }
}

fn u64_at(frame: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(frame[at..at + 8].try_into().expect("8-byte slice"))
}

fn i64_at(frame: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(frame[at..at + 8].try_into().expect("8-byte slice"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_order_layout_is_stable() {
        let order = NewOrder::limit(0x0102_0304_0506_0708, Side::Sell, 10_050, 25)
            .account(7)
            .post_only()
            .stp(StpMode::CancelOldest);
        let mut buf = Vec::new();
        encode(&Command::New(order), &mut buf);
        assert_eq!(buf.len(), NEW_LEN);
        assert_eq!(&buf[..8], &[1, 1, 0, 0, 1, 2, 0, 0]);
        assert_eq!(&buf[8..16], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&buf[16..20], &[7, 0, 0, 0]);
        assert_eq!(&buf[24..32], &10_050i64.to_le_bytes());
        assert_eq!(&buf[32..40], &25u64.to_le_bytes());
        assert_eq!(decode(&buf), Ok((Command::New(order), NEW_LEN)));
    }

    #[test]
    fn rejects_malformed_frames() {
        let mut buf = Vec::new();
        encode(&Command::Cancel { id: 9 }, &mut buf);
        assert_eq!(
            decode(&buf[..10]),
            Err(DecodeError::Truncated {
                needed: CANCEL_LEN,
                available: 10
            })
        );
        buf[3] = 1;
        assert_eq!(decode(&buf), Err(DecodeError::NonZeroReserved));
        assert_eq!(decode(&[9]), Err(DecodeError::UnknownType(9)));
        assert_eq!(
            decode(&[]),
            Err(DecodeError::Truncated {
                needed: 1,
                available: 0
            })
        );

        let mut market = Vec::new();
        encode(
            &Command::New(NewOrder::market(1, Side::Buy, 5)),
            &mut market,
        );
        market[24] = 1;
        assert_eq!(decode(&market), Err(DecodeError::MarketPrice));
        market[24] = 0;
        market[1] = 2;
        assert_eq!(
            decode(&market),
            Err(DecodeError::InvalidField {
                field: "side",
                value: 2
            })
        );
    }
}
