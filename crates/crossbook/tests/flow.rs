//! Long synthetic sessions: differential checks at scale, journal replay and golden digests,
//! for continuous trading and for trading days with opening and closing auctions.

mod common;

use crossbook::flow::{Flow, FlowConfig, Schedule};
use crossbook::reference::ReferenceEngine;
use crossbook::{Command, Engine, Event, Phase, digest, journal};

fn commands(config: FlowConfig, count: usize) -> Vec<Command> {
    Flow::new(config).take(count).collect()
}

fn run(commands: &[Command]) -> (u64, u64, usize) {
    let mut engine = Engine::new();
    let mut events = Vec::new();
    let mut fingerprint = digest::Fingerprint::new();
    for command in commands {
        events.clear();
        engine.process(command, &mut events);
        for event in &events {
            fingerprint.event(event);
        }
    }
    (fingerprint.value(), engine.state_hash(), engine.len())
}

#[test]
fn synthetic_flow_matches_reference() {
    let count = std::env::var("CROSSBOOK_FLOW_COMMANDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40_000);
    for seed in [1, 2, 3] {
        let config = FlowConfig {
            target_orders: 400,
            ..FlowConfig::seeded(seed)
        };
        let mut fast = Engine::new();
        let mut reference = ReferenceEngine::new();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for (index, command) in commands(config, count).iter().enumerate() {
            a.clear();
            b.clear();
            fast.process(command, &mut a);
            reference.process(command, &mut b);
            assert_eq!(a, b, "seed {seed}, command {index}: {command:?}");
            if index.is_multiple_of(500) {
                fast.check_invariants().unwrap();
                assert_eq!(
                    fast.snapshot(),
                    reference.snapshot(),
                    "seed {seed}, command {index}"
                );
            }
        }
        assert_eq!(fast.snapshot(), reference.snapshot());
    }
}

#[test]
fn auction_sessions_match_reference() {
    let count = std::env::var("CROSSBOOK_FLOW_COMMANDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25_000);
    let schedule = Schedule {
        opening_call: 1_000,
        continuous: 8_000,
        closing_call: 1_000,
    };
    for seed in [1, 2, 3] {
        let config = FlowConfig {
            target_orders: 400,
            session: Some(schedule),
            ..FlowConfig::seeded(seed)
        };
        let mut fast = Engine::new();
        let mut reference = ReferenceEngine::new();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let (mut executed, mut scheduled) = (0, 0);
        for (index, command) in commands(config, count).iter().enumerate() {
            scheduled += usize::from(matches!(command, Command::Uncross));
            let uncross = matches!(command, Command::Uncross | Command::Resume);
            let before = uncross.then(|| (fast.snapshot(), fast.reference_price()));
            a.clear();
            b.clear();
            fast.process(command, &mut a);
            reference.process(command, &mut b);
            assert_eq!(a, b, "seed {seed}, command {index}: {command:?}");
            if let Some((book, price)) = before {
                common::check_uncross(&book, price, &a, &fast)
                    .unwrap_or_else(|error| panic!("seed {seed}, command {index}: {error}"));
                executed += a
                    .iter()
                    .filter(|event| matches!(event, Event::Uncrossed { volume, .. } if *volume > 0))
                    .count();
            }
            if index.is_multiple_of(500) || uncross {
                fast.check_invariants().unwrap();
                assert_eq!(
                    fast.snapshot(),
                    reference.snapshot(),
                    "seed {seed}, command {index}"
                );
                assert_eq!(fast.indicative_uncross(), reference.indicative_uncross());
            }
        }
        assert_eq!(fast.snapshot(), reference.snapshot());
        assert_eq!(fast.phase(), reference.phase());
        // Call phases always cross, so every scheduled uncross trades (25,000 commands
        // hold five of them).
        assert!(scheduled > 0);
        assert_eq!(executed, scheduled, "seed {seed}: uncrosses that traded");
    }
}

#[test]
fn schedule_places_the_phase_commands() {
    let schedule = Schedule {
        opening_call: 3,
        continuous: 4,
        closing_call: 2,
    };
    let config = FlowConfig {
        session: Some(schedule),
        ..FlowConfig::seeded(9)
    };
    let mut flow = Flow::new(config);
    let day: Vec<Command> = flow.by_ref().take(2 * schedule.day_len()).collect();
    let phase_commands: Vec<(usize, Command)> = day
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, command)| {
            !matches!(
                command,
                Command::New(_) | Command::Cancel { .. } | Command::Modify { .. }
            )
        })
        .collect();
    let start = |reference| Command::StartCall { reference };
    assert_eq!(
        phase_commands,
        [
            (0, start(Some(10_000))),
            (4, Command::Uncross),
            (5, Command::Resume),
            (10, start(None)),
            (13, Command::Uncross),
            (14, start(None)),
            (18, Command::Uncross),
            (19, Command::Resume),
            (24, start(None)),
            (27, Command::Uncross),
        ]
    );
    assert_eq!(flow.book().phase(), Phase::Call);
    // Call phases send only GTC orders, which a call phase accepts.
    for command in &day[1..4] {
        if let Command::New(order) = command {
            assert_eq!(order.tif, crossbook::TimeInForce::Gtc);
            assert!(!order.post_only);
        }
    }
}

#[test]
fn journal_replay_reproduces_the_session() {
    let commands = commands(FlowConfig::seeded(11), 25_000);
    let mut bytes = Vec::new();
    journal::write(&mut bytes, &commands).unwrap();
    let replayed: Vec<Command> = journal::Reader::new(&bytes[..])
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(replayed, commands);
    assert_eq!(run(&replayed), run(&commands));
}

/// Pins the exact output for the default flow. The browser playground shows the same
/// numbers after compiling the engine to WebAssembly. If a deliberate change to the
/// engine or the generator moves them, update them in the same commit and say why.
#[test]
fn golden_digests_for_seed_42() {
    let (events, state, resting) = run(&commands(FlowConfig::seeded(42), 100_000));
    assert_eq!(
        (format!("{events:016x}"), format!("{state:016x}"), resting),
        (
            GOLDEN_EVENTS.to_owned(),
            GOLDEN_STATE.to_owned(),
            GOLDEN_RESTING
        )
    );
}

const GOLDEN_EVENTS: &str = "3749b835b0ff0cae";
const GOLDEN_STATE: &str = "ee0bfb94bbfc39a7";
const GOLDEN_RESTING: usize = 9_124;

/// The same for 200,000 commands of trading days with auctions: two opening auctions, one
/// closing auction, and the second day's closing call still collecting orders. The
/// WebAssembly test pins the same numbers.
#[test]
fn golden_digests_for_an_auction_session() {
    let config = FlowConfig {
        session: Some(Schedule::default()),
        ..FlowConfig::seeded(42)
    };
    let (events, state, resting) = run(&commands(config, 200_000));
    assert_eq!(
        (format!("{events:016x}"), format!("{state:016x}"), resting),
        (
            AUCTION_EVENTS.to_owned(),
            AUCTION_STATE.to_owned(),
            AUCTION_RESTING
        )
    );
}

const AUCTION_EVENTS: &str = "a0e26dd141f0f853";
const AUCTION_STATE: &str = "a0fbd4956bedd22b";
const AUCTION_RESTING: usize = 9_347;
