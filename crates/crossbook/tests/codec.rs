//! Wire codec properties: every command round-trips, and arbitrary bytes never panic.

use crossbook::codec::{decode, encode, encoded_len};
use crossbook::{Command, NewOrder, OrderKind, Side, StpMode, TimeInForce};
use proptest::prelude::*;

fn command() -> impl Strategy<Value = Command> {
    let side = prop_oneof![Just(Side::Buy), Just(Side::Sell)];
    let kind = prop_oneof![
        any::<i64>().prop_map(OrderKind::Limit),
        Just(OrderKind::Market)
    ];
    let tif = prop_oneof![
        Just(TimeInForce::Gtc),
        Just(TimeInForce::Ioc),
        Just(TimeInForce::Fok)
    ];
    let stp = prop_oneof![
        Just(StpMode::Allow),
        Just(StpMode::CancelNewest),
        Just(StpMode::CancelOldest),
        Just(StpMode::CancelBoth),
    ];
    let new = (
        any::<u64>(),
        any::<u32>(),
        side,
        kind,
        any::<u64>(),
        tif,
        any::<bool>(),
        stp,
    )
        .prop_map(|(id, account, side, kind, qty, tif, post_only, stp)| {
            Command::New(NewOrder {
                id,
                account,
                side,
                kind,
                qty,
                tif,
                post_only,
                stp,
            })
        });
    prop_oneof![
        new,
        any::<u64>().prop_map(|id| Command::Cancel { id }),
        (any::<u64>(), any::<i64>(), any::<u64>()).prop_map(|(id, price, qty)| Command::Modify {
            id,
            price,
            qty
        }),
    ]
}

proptest! {
    #[test]
    fn every_command_round_trips(command in command(), trailing in prop::collection::vec(any::<u8>(), 0..8)) {
        let mut buf = Vec::new();
        encode(&command, &mut buf);
        prop_assert_eq!(buf.len(), encoded_len(&command));
        buf.extend_from_slice(&trailing);
        prop_assert_eq!(decode(&buf), Ok((command, encoded_len(&command))));
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        if let Ok((command, len)) = decode(&bytes) {
            let mut again = Vec::new();
            encode(&command, &mut again);
            prop_assert_eq!(&again[..], &bytes[..len], "decoding accepted a non-canonical frame");
        }
    }

    #[test]
    fn plausible_frames_decode_canonically(kind in 1u8..=3, mut body in prop::collection::vec(prop_oneof![9 => Just(0u8), 1 => any::<u8>()], 39)) {
        // Mostly-zero bodies reach the field checks far more often than uniform bytes do.
        body.insert(0, kind);
        if let Ok((command, len)) = decode(&body) {
            let mut again = Vec::new();
            encode(&command, &mut again);
            prop_assert_eq!(&again[..], &body[..len]);
        }
    }
}
