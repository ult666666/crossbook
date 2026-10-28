//! Criterion micro-benchmarks. Run with `cargo bench -p crossbook`.
//!
//! Each benchmark starts from a realistic book: the state after 100,000 commands of the
//! default synthetic flow (about 9,000 resting orders across a few dozen price levels).

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use crossbook::flow::{Flow, FlowConfig};
use crossbook::reference::ReferenceEngine;
use crossbook::{Command, Engine, NewOrder, Side};

fn warm_book() -> (Engine, Vec<Command>) {
    let commands: Vec<Command> = Flow::new(FlowConfig::seeded(42)).take(100_000).collect();
    let mut engine = Engine::new();
    let mut events = Vec::new();
    for command in &commands {
        engine.process(command, &mut events);
        events.clear();
    }
    (engine, commands)
}

fn operations(c: &mut Criterion) {
    let (book, _) = warm_book();
    let bid = book.best_bid().expect("warm book has bids").price;
    let ask = book.best_ask().expect("warm book has asks").price;
    let mut group = c.benchmark_group("operations");

    // A passive order that rests behind the best bid, then its cancel: the book is the
    // same before and after, so every iteration sees identical state.
    group.bench_function("add and cancel passive order", |b| {
        let mut engine = book.clone();
        let mut events = Vec::with_capacity(8);
        let resting = NewOrder::limit(u64::MAX - 1, Side::Buy, bid - 2, 10);
        b.iter(|| {
            events.clear();
            engine.process(black_box(&Command::New(resting)), &mut events);
            engine.process(black_box(&Command::Cancel { id: resting.id }), &mut events);
        });
    });

    group.bench_function("cancel from middle of queue", |b| {
        let victim = book.snapshot()[book.len() / 2].id;
        b.iter_batched_ref(
            || (book.clone(), Vec::with_capacity(4)),
            |(engine, events)| engine.process(black_box(&Command::Cancel { id: victim }), events),
            BatchSize::LargeInput,
        );
    });

    group.bench_function("market order sweeping 10 levels", |b| {
        let depth: u64 = book.depth(10).asks.iter().map(|level| level.qty).sum();
        let sweep = NewOrder::market(u64::MAX - 2, Side::Buy, depth);
        b.iter_batched_ref(
            || (book.clone(), Vec::with_capacity(4_096)),
            |(engine, events)| engine.process(black_box(&Command::New(sweep)), events),
            BatchSize::LargeInput,
        );
    });

    group.bench_function("aggressive limit filling one order", |b| {
        let take = NewOrder::limit(u64::MAX - 3, Side::Buy, ask, 1);
        b.iter_batched_ref(
            || (book.clone(), Vec::with_capacity(8)),
            |(engine, events)| engine.process(black_box(&Command::New(take)), events),
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn sessions(c: &mut Criterion) {
    let commands: Vec<Command> = Flow::new(FlowConfig::seeded(7)).take(100_000).collect();
    let mut group = c.benchmark_group("sessions");
    group.throughput(Throughput::Elements(commands.len() as u64));
    group.sample_size(20);
    group.bench_function("fast engine, 100k synthetic commands", |b| {
        b.iter(|| {
            let mut engine = Engine::new();
            let mut events = Vec::with_capacity(64);
            for command in &commands {
                events.clear();
                engine.process(command, &mut events);
            }
            black_box(engine.state_hash())
        });
    });

    // The reference model does linear scans; run it on a small book so it finishes.
    let small: Vec<Command> = Flow::new(FlowConfig {
        target_orders: 500,
        ..FlowConfig::seeded(7)
    })
    .take(10_000)
    .collect();
    group.throughput(Throughput::Elements(small.len() as u64));
    group.bench_function("fast engine, 10k commands, 500-order book", |b| {
        b.iter(|| {
            let mut engine = Engine::new();
            let mut events = Vec::with_capacity(64);
            for command in &small {
                events.clear();
                engine.process(command, &mut events);
            }
            black_box(engine.state_hash())
        });
    });
    group.bench_function("reference model, 10k commands, 500-order book", |b| {
        b.iter(|| {
            let mut engine = ReferenceEngine::new();
            let mut events = Vec::with_capacity(64);
            for command in &small {
                events.clear();
                engine.process(command, &mut events);
            }
            black_box(engine.len())
        });
    });
    group.finish();
}

criterion_group!(benches, operations, sessions);
criterion_main!(benches);
