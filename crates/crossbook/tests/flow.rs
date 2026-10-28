//! Long synthetic sessions: differential checks at scale, journal replay and golden digests.

use crossbook::flow::{Flow, FlowConfig};
use crossbook::reference::ReferenceEngine;
use crossbook::{Command, Engine, digest, journal};

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
