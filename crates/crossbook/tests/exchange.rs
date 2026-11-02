//! Sharded-exchange property tests.
//!
//! Spreading instruments over threads must not change anything a participant can see:
//! for any shard count, batch size and queue length, [`exchange::run`] must deliver
//! exactly the events that one thread processing the commands in order produces, in the
//! same order. The commands include each instrument's own call phases and uncrosses.

mod common;

use std::collections::HashMap;

use common::{config, session_command};
use crossbook::exchange::{self, Config, Symbol};
use crossbook::flow::{Flow, FlowConfig, Schedule, SplitMix64};
use crossbook::{Command, Engine, Event, digest::Fingerprint};
use proptest::prelude::*;

fn one_thread(commands: &[(Symbol, Command)]) -> Vec<(u64, Symbol, Vec<Event>)> {
    let mut books: HashMap<Symbol, Engine> = HashMap::new();
    let mut out = Vec::new();
    for (seq, &(symbol, command)) in (0u64..).zip(commands) {
        let mut events = Vec::new();
        books
            .entry(symbol)
            .or_default()
            .process(&command, &mut events);
        out.push((seq, symbol, events));
    }
    out
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn sharded_output_equals_one_thread(
        commands in prop::collection::vec((0u32..7, session_command()), 1..300),
        shards in 1usize..6,
        batch in 1usize..40,
        queue in 1usize..4,
    ) {
        let mut sharded = Vec::new();
        let stats = exchange::run(commands.clone(), Config { shards, batch, queue }, |output| {
            sharded.push((output.seq, output.symbol, output.events.to_vec()));
        });
        let expected = one_thread(&commands);
        prop_assert_eq!(stats.commands, commands.len() as u64);
        prop_assert_eq!(stats.batches, commands.len().div_ceil(batch) as u64);
        prop_assert_eq!(
            stats.events,
            expected.iter().map(|(_, _, events)| events.len() as u64).sum::<u64>()
        );
        prop_assert_eq!(sharded, expected);
    }
}

/// Many deep books with realistic flow, each with its own opening and closing auctions:
/// the same digest for every shard count.
#[test]
fn synthetic_multi_instrument_session_is_shard_independent() {
    let symbols = 24u32;
    let session = Some(Schedule {
        opening_call: 300,
        continuous: 5_000,
        closing_call: 300,
    });
    let mut flows: Vec<Flow> = (0..symbols)
        .map(|symbol| {
            Flow::new(FlowConfig {
                session,
                ..FlowConfig::seeded(1_000 + u64::from(symbol))
            })
        })
        .collect();
    let mut pick = SplitMix64::new(99);
    let commands: Vec<(Symbol, Command)> = (0..150_000)
        .map(|_| {
            let symbol = pick.below(u64::from(symbols)) as Symbol;
            (
                symbol,
                flows[symbol as usize].next().expect("flow is endless"),
            )
        })
        .collect();
    let digest = |shards: usize, batch: usize| {
        let mut fingerprint = Fingerprint::new();
        exchange::run(
            commands.iter().copied(),
            Config {
                shards,
                batch,
                queue: 4,
            },
            |output| {
                fingerprint.u64(output.seq);
                fingerprint.u32(output.symbol);
                for event in output.events {
                    fingerprint.event(event);
                }
            },
        );
        fingerprint.value()
    };
    let auctions = one_thread(&commands)
        .iter()
        .flat_map(|(_, _, events)| events)
        .filter(|event| matches!(event, Event::Uncrossed { volume, .. } if *volume > 0))
        .count();
    assert!(
        auctions >= 2 * symbols as usize,
        "only {auctions} uncrosses executed"
    );
    let reference = digest(1, 1024);
    for (shards, batch) in [(2, 1024), (3, 100), (4, 4096), (7, 1)] {
        assert_eq!(
            digest(shards, batch),
            reference,
            "{shards} shards, batch {batch}"
        );
    }
}
