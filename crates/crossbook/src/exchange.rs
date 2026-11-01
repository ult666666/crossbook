//! Many instruments on several threads, with one deterministic output.
//!
//! A venue runs one book per instrument. The books are independent, so they can run in
//! parallel, but participants still need a single order of events. [`run`] follows the
//! classic design:
//!
//! 1. A sequencer thread numbers the commands and cuts the stream into batches of
//!    consecutive commands. It sends each shard the batch's commands for the instruments
//!    the shard owns, and sends the merger the batch's routing: the instrument of each
//!    command, in order.
//! 2. Each shard thread owns the books of its instruments and processes its commands in
//!    arrival order.
//! 3. The merger, on the calling thread, follows the routing to put the shards' output
//!    back in sequence order and hands each command's events to the caller.
//!
//! The output is identical to processing every command on one thread, for any number of
//! shards and any batch size; the tests check this. Queues are bounded, so a slow
//! consumer slows the sequencer down instead of growing memory without limit. Batching
//! and bounded queues can deadlock an in-order merge when a batch waits for later work.
//! Here every batch is complete before the next one starts and the merger always waits
//! for the oldest unfinished batch. That batch's commands are already queued at the
//! shards ahead of any later work, so some thread can always make progress.
//!
//! Buffers travel in a loop instead of being allocated per batch: command parts go back
//! from the shards to the sequencer, event parts from the merger to their shard, and
//! routes from the merger to the sequencer. Returns never block (a full return queue
//! drops the buffer), so they add no waiting to the argument above.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

use crate::Engine;
use crate::types::{Command, Event};

/// Instrument identifier. Each instrument has its own book.
pub type Symbol = u32;

/// How [`run`] spreads the work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Worker threads that own books. Instrument `s` belongs to shard `s % shards`.
    pub shards: usize,
    /// Consecutive commands per batch. Larger batches cost fewer queue operations and
    /// more latency.
    pub batch: usize,
    /// Batches each queue holds before its sender waits.
    pub queue: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shards: 4,
            batch: 1024,
            queue: 8,
        }
    }
}

/// One command's result, delivered in sequence order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output<'a> {
    /// Position of the command in the input, from 0.
    pub seq: u64,
    /// Instrument the command was for.
    pub symbol: Symbol,
    /// The events of that instrument's engine for the command.
    pub events: &'a [Event],
}

/// Totals for a [`run`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Commands processed.
    pub commands: u64,
    /// Events delivered.
    pub events: u64,
    /// Batches the sequencer cut.
    pub batches: u64,
}

/// One shard's results for its part of a batch.
#[derive(Default)]
struct Part {
    /// Start and length in `events` for each command, in arrival order.
    spans: Vec<(u32, u32)>,
    events: Vec<Event>,
}

type Commands = Vec<(Symbol, Command)>;

/// Multiplicative hasher for instrument ids, which are trusted small integers.
#[derive(Clone, Copy, Default)]
struct SymbolHasher(u64);

impl Hasher for SymbolHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type Books = HashMap<Symbol, Engine, BuildHasherDefault<SymbolHasher>>;

/// Takes a recycled buffer if one is waiting.
fn reuse<T: Default>(returns: &Receiver<T>) -> T {
    returns.try_recv().unwrap_or_default()
}

/// Processes `commands` across `config.shards` worker threads and calls `sink` with each
/// command's events in input order.
///
/// # Panics
/// Panics if `config` has zero shards, batch size or queue length, and propagates a
/// panic from `sink` or from a worker thread.
pub fn run<I>(commands: I, config: Config, mut sink: impl FnMut(Output<'_>)) -> Stats
where
    I: IntoIterator<Item = (Symbol, Command)>,
    I::IntoIter: Send,
{
    assert!(
        config.shards > 0 && config.batch > 0 && config.queue > 0,
        "shards, batch and queue must be positive: {config:?}"
    );
    let commands = commands.into_iter();
    // Return queues hold every buffer that can be in flight, so returns rarely drop.
    let spare = 2 * config.queue + 2;
    thread::scope(|scope| {
        let (spent_commands, reused_commands) = sync_channel::<Commands>(spare * config.shards);
        let (spent_routes, reused_routes) = sync_channel::<Vec<Symbol>>(spare);
        let mut inputs = Vec::with_capacity(config.shards);
        let mut outputs = Vec::with_capacity(config.shards);
        let mut part_returns = Vec::with_capacity(config.shards);
        for _ in 0..config.shards {
            let (input, shard_input) = sync_channel(config.queue);
            let (shard_output, output) = sync_channel(config.queue);
            let (part_return, reused_parts) = sync_channel(spare);
            let spent_commands = spent_commands.clone();
            scope.spawn(move || {
                shard(&shard_input, &shard_output, &reused_parts, &spent_commands);
            });
            inputs.push(input);
            outputs.push(output);
            part_returns.push(part_return);
        }
        drop(spent_commands);
        let (routes, merger_routes) = sync_channel(config.queue);
        scope.spawn(move || {
            sequence(
                commands,
                config.batch,
                &inputs,
                &routes,
                &reused_commands,
                &reused_routes,
            );
        });
        merge(
            &merger_routes,
            &outputs,
            &part_returns,
            &spent_routes,
            &mut sink,
        )
    })
}

const fn shard_of(symbol: Symbol, shards: usize) -> usize {
    symbol as usize % shards
}

/// The sequencer: cuts batches, routes each command to its shard, tells the merger.
fn sequence(
    commands: impl Iterator<Item = (Symbol, Command)>,
    batch: usize,
    inputs: &[SyncSender<Commands>],
    routes: &SyncSender<Vec<Symbol>>,
    reused_commands: &Receiver<Commands>,
    reused_routes: &Receiver<Vec<Symbol>>,
) {
    let shards = inputs.len();
    let mut parts: Vec<Commands> = (0..shards).map(|_| Vec::new()).collect();
    let mut route = Vec::with_capacity(batch);
    for (symbol, command) in commands {
        parts[shard_of(symbol, shards)].push((symbol, command));
        route.push(symbol);
        if route.len() == batch {
            for (part, input) in parts.iter_mut().zip(inputs) {
                if !part.is_empty() {
                    let next = reuse(reused_commands);
                    if input.send(std::mem::replace(part, next)).is_err() {
                        return;
                    }
                }
            }
            let next = reuse(reused_routes);
            if routes.send(std::mem::replace(&mut route, next)).is_err() {
                return;
            }
        }
    }
    if !route.is_empty() {
        for (part, input) in parts.into_iter().zip(inputs) {
            if !part.is_empty() && input.send(part).is_err() {
                return;
            }
        }
        let _ = routes.send(route);
    }
}

/// A shard: owns the books of its instruments and processes its commands in order.
fn shard(
    input: &Receiver<Commands>,
    output: &SyncSender<Part>,
    reused_parts: &Receiver<Part>,
    spent_commands: &SyncSender<Commands>,
) {
    let mut books = Books::default();
    while let Ok(mut commands) = input.recv() {
        let mut part = reuse(reused_parts);
        part.spans.clear();
        part.events.clear();
        for &(symbol, command) in &commands {
            let start = part.events.len();
            books
                .entry(symbol)
                .or_default()
                .process(&command, &mut part.events);
            let span = |n: usize| u32::try_from(n).expect("batch events fit u32");
            part.spans
                .push((span(start), span(part.events.len() - start)));
        }
        if output.send(part).is_err() {
            return;
        }
        commands.clear();
        let _ = spent_commands.try_send(commands);
    }
}

/// The merger: follows each batch's routing to restore sequence order.
fn merge(
    routes: &Receiver<Vec<Symbol>>,
    outputs: &[Receiver<Part>],
    part_returns: &[SyncSender<Part>],
    spent_routes: &SyncSender<Vec<Symbol>>,
    sink: &mut impl FnMut(Output<'_>),
) -> Stats {
    let shards = outputs.len();
    let mut current: Vec<Option<(Part, usize)>> = (0..shards).map(|_| None).collect();
    let mut stats = Stats::default();
    while let Ok(mut route) = routes.recv() {
        stats.batches += 1;
        for &symbol in &route {
            let shard = shard_of(symbol, shards);
            let (part, cursor) = current[shard].get_or_insert_with(|| {
                let part = outputs[shard].recv().expect("a shard thread stopped early");
                (part, 0)
            });
            let (start, len) = part.spans[*cursor];
            let events = &part.events[start as usize..(start + len) as usize];
            sink(Output {
                seq: stats.commands,
                symbol,
                events,
            });
            stats.commands += 1;
            stats.events += u64::from(len);
            *cursor += 1;
            if *cursor == part.spans.len()
                && let Some((part, _)) = current[shard].take()
            {
                let _ = part_returns[shard].try_send(part);
            }
        }
        route.clear();
        let _ = spent_routes.try_send(route);
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{NewOrder, Side};

    #[test]
    fn instruments_are_independent_and_output_stays_in_order() {
        let commands = vec![
            (0, Command::New(NewOrder::limit(1, Side::Sell, 100, 5))),
            (1, Command::New(NewOrder::limit(1, Side::Sell, 100, 5))),
            (0, Command::New(NewOrder::limit(2, Side::Buy, 100, 2))),
            (2, Command::Cancel { id: 1 }),
            (1, Command::Cancel { id: 1 }),
        ];
        for shards in 1..=3 {
            let mut seen = Vec::new();
            let stats = run(
                commands.clone(),
                Config {
                    shards,
                    batch: 2,
                    queue: 1,
                },
                |output| seen.push((output.seq, output.symbol, output.events.to_vec())),
            );
            assert_eq!(stats.commands, 5);
            assert_eq!(stats.batches, 3);
            let seqs: Vec<u64> = seen.iter().map(|(seq, _, _)| *seq).collect();
            assert_eq!(seqs, [0, 1, 2, 3, 4]);
            // Symbol 0 traded; symbol 2 never saw order 1; symbol 1 canceled its own.
            assert!(matches!(seen[2].2[1], Event::Trade { qty: 2, .. }));
            assert!(matches!(seen[3].2[..], [Event::Rejected { id: 1, .. }]));
            assert!(matches!(seen[4].2[..], [Event::Canceled { id: 1, .. }]));
        }
    }

    #[test]
    fn empty_input_finishes() {
        let stats = run(Vec::new(), Config::default(), |_| unreachable!());
        assert_eq!(stats, Stats::default());
    }
}
