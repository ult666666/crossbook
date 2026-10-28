//! `crossbook`: generate, replay, verify and benchmark synthetic order flow.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crossbook::flow::{Flow, FlowConfig};
use crossbook::reference::ReferenceEngine;
use crossbook::{Command, Engine, Event, digest, journal};
use hdrhistogram::Histogram;

const USAGE: &str = "\
crossbook: deterministic limit order book and matching engine

USAGE:
    crossbook gen     --out FILE [--orders N] [--seed S] [--book N]
    crossbook replay  FILE [--depth N]
    crossbook verify  [--orders N] [--seed S] [--book N]
    crossbook bench   [--orders N] [--seed S] [--book N] [--json]
    crossbook demo

COMMANDS:
    gen      Write N synthetic commands to a journal file
    replay   Run a journal through the engine and print the result and its fingerprints
    verify   Run synthetic flow through the fast engine and the reference model and
             require identical events and books after every command
    bench    Measure throughput and per-command latency on synthetic flow
    demo     Walk through a short annotated session

Defaults: --orders 1000000, --seed 42, --depth 5, --book 10000 (target resting orders;
verify defaults to 1000 because the reference model scans the whole book on every
command). Flow is synthetic, not market data.";

struct Args {
    positional: Vec<String>,
    orders: usize,
    seed: u64,
    depth: usize,
    book: Option<usize>,
    out: Option<String>,
    json: bool,
}

fn parse(mut raw: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args {
        positional: Vec::new(),
        orders: 1_000_000,
        seed: 42,
        depth: 5,
        book: None,
        out: None,
        json: false,
    };
    while let Some(arg) = raw.next() {
        let mut value = |name: &str| raw.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--orders" | "-n" => args.orders = number(&value("--orders")?)?,
            "--seed" => args.seed = number(&value("--seed")?)?,
            "--depth" => args.depth = number(&value("--depth")?)?,
            "--book" => args.book = Some(number(&value("--book")?)?),
            "--out" | "-o" => args.out = Some(value("--out")?),
            "--json" => args.json = true,
            "-h" | "--help" => args.positional.insert(0, "help".to_owned()),
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            _ => args.positional.push(arg),
        }
    }
    Ok(args)
}

fn number<T: std::str::FromStr>(text: &str) -> Result<T, String> {
    text.replace('_', "")
        .parse()
        .map_err(|_| format!("not a number: {text}"))
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match args.positional.first().map(String::as_str) {
        Some("gen") => generate(&args),
        Some("replay") => replay(&args),
        Some("verify") => verify(&args),
        Some("bench") => bench(&args),
        Some("demo") => {
            demo();
            Ok(())
        }
        Some("help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command {other}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn synthetic(args: &Args, default_book: usize) -> Vec<Command> {
    let config = FlowConfig {
        target_orders: args.book.unwrap_or(default_book),
        ..FlowConfig::seeded(args.seed)
    };
    Flow::new(config).take(args.orders).collect()
}

fn generate(args: &Args) -> Result<(), String> {
    let path = args.out.as_deref().ok_or("gen needs --out FILE")?;
    let commands = synthetic(args, 10_000);
    let file = File::create(path).map_err(|error| format!("{path}: {error}"))?;
    journal::write(BufWriter::new(file), &commands).map_err(|error| format!("{path}: {error}"))?;
    println!(
        "wrote {} commands (seed {}) to {path}",
        group(commands.len() as u64),
        args.seed
    );
    Ok(())
}

#[derive(Default)]
struct Tally {
    accepted: u64,
    rejected: u64,
    trades: u64,
    volume: u64,
    booked: u64,
    canceled: u64,
    modified: u64,
}

impl Tally {
    fn add(&mut self, event: &Event) {
        match event {
            Event::Accepted { .. } => self.accepted += 1,
            Event::Rejected { .. } => self.rejected += 1,
            Event::Trade { qty, .. } => {
                self.trades += 1;
                self.volume += qty;
            }
            Event::Booked { .. } => self.booked += 1,
            Event::Canceled { .. } => self.canceled += 1,
            Event::Modified { .. } => self.modified += 1,
        }
    }
}

fn replay(args: &Args) -> Result<(), String> {
    let path = args
        .positional
        .get(1)
        .ok_or("replay needs a journal FILE")?;
    let file = File::open(path).map_err(|error| format!("{path}: {error}"))?;
    let reader =
        journal::Reader::new(BufReader::new(file)).map_err(|error| format!("{path}: {error}"))?;

    let mut engine = Engine::new();
    let mut events = Vec::new();
    let mut fingerprint = digest::Fingerprint::new();
    let mut tally = Tally::default();
    let mut commands = 0u64;
    let mut busy = Duration::ZERO;
    for command in reader {
        let command = command.map_err(|error| format!("{path}: {error}"))?;
        events.clear();
        let started = Instant::now();
        engine.process(&command, &mut events);
        busy += started.elapsed();
        commands += 1;
        for event in &events {
            fingerprint.event(event);
            tally.add(event);
        }
    }

    println!("journal         {path}");
    println!("commands        {}", group(commands));
    println!(
        "events          accepted {}, rejected {}, trades {}, booked {}, canceled {}, modified {}",
        group(tally.accepted),
        group(tally.rejected),
        group(tally.trades),
        group(tally.booked),
        group(tally.canceled),
        group(tally.modified)
    );
    println!("traded volume   {} lots", group(tally.volume));
    println!(
        "resting orders  {} ({} bid levels, {} ask levels)",
        group(engine.len() as u64),
        engine.level_count(crossbook::Side::Buy),
        engine.level_count(crossbook::Side::Sell)
    );
    print_depth(&engine, args.depth);
    println!("event digest    {:016x}", fingerprint.value());
    println!("state hash      {:016x}", engine.state_hash());
    println!(
        "engine time     {:.3} s ({} commands/s)",
        busy.as_secs_f64(),
        rate(commands, busy)
    );
    Ok(())
}

fn print_depth(engine: &Engine, levels: usize) {
    let depth = engine.depth(levels);
    println!(
        "depth           {:>12}  {:>8}  |  {:<8}  {:<12}",
        "bid qty", "bid", "ask", "ask qty"
    );
    for row in 0..levels {
        let bid = depth.bids.get(row);
        let ask = depth.asks.get(row);
        if bid.is_none() && ask.is_none() {
            break;
        }
        let cell = |level: Option<&crossbook::LevelView>,
                    f: fn(&crossbook::LevelView) -> String| {
            level.map(f).unwrap_or_default()
        };
        println!(
            "                {:>12}  {:>8}  |  {:<8}  {:<12}",
            cell(bid, |l| group(l.qty)),
            cell(bid, |l| l.price.to_string()),
            cell(ask, |l| l.price.to_string()),
            cell(ask, |l| group(l.qty)),
        );
    }
}

fn verify(args: &Args) -> Result<(), String> {
    let commands = synthetic(args, 1_000);
    let mut fast = Engine::new();
    let mut reference = ReferenceEngine::new();
    let (mut fast_events, mut reference_events) = (Vec::new(), Vec::new());
    let started = Instant::now();
    for (index, command) in commands.iter().enumerate() {
        fast_events.clear();
        reference_events.clear();
        fast.process(command, &mut fast_events);
        reference.process(command, &mut reference_events);
        if fast_events != reference_events {
            return Err(format!(
                "engines diverged at command {index} ({command:?}):\n  fast:      {fast_events:?}\n  reference: {reference_events:?}"
            ));
        }
        if index.is_multiple_of(1024) || index + 1 == commands.len() {
            fast.check_invariants()
                .map_err(|error| format!("invariant broken at command {index}: {error}"))?;
            if fast.snapshot() != reference.snapshot() {
                return Err(format!("books diverged at command {index}"));
            }
        }
    }
    println!(
        "ok: fast engine and reference model agree on {} commands (seed {}), final book {} orders, state hash {:016x} [{:.1} s]",
        group(commands.len() as u64),
        args.seed,
        group(fast.len() as u64),
        fast.state_hash(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn bench(args: &Args) -> Result<(), String> {
    let commands = synthetic(args, 10_000);
    let count = commands.len() as u64;
    if count == 0 {
        return Err("--orders must be positive".to_owned());
    }

    // Throughput: one untimed warm-up pass, then the best of three timed passes.
    let mut events = Vec::with_capacity(64);
    let mut best = Duration::MAX;
    let mut final_hash = 0;
    let mut event_count = 0u64;
    for pass in 0..4 {
        let mut engine = Engine::with_capacity(1 << 16);
        let started = Instant::now();
        for command in &commands {
            events.clear();
            engine.process(command, &mut events);
            event_count += events.len() as u64;
        }
        let elapsed = started.elapsed();
        if pass > 0 {
            best = best.min(elapsed);
        }
        final_hash = engine.state_hash();
    }
    let events_per_pass = event_count / 4;

    // Latency: time every command individually and subtract the clock-read overhead.
    let overhead = timer_overhead();
    let mut histogram =
        Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3).map_err(|e| e.to_string())?;
    let mut engine = Engine::with_capacity(1 << 16);
    for command in &commands {
        events.clear();
        let started = Instant::now();
        engine.process(command, &mut events);
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        histogram.saturating_record(nanos.saturating_sub(overhead).max(1));
    }

    let throughput = count as f64 / best.as_secs_f64();
    let percentiles = [50.0, 90.0, 99.0, 99.9, 99.99];
    if args.json {
        let quantiles: Vec<String> = percentiles
            .iter()
            .map(|p| {
                format!(
                    "\"p{}\": {}",
                    p.to_string().replace('.', "_"),
                    histogram.value_at_quantile(p / 100.0)
                )
            })
            .collect();
        println!(
            "{{\"commands\": {count}, \"seed\": {}, \"events\": {events_per_pass}, \"best_seconds\": {:.6}, \"commands_per_second\": {:.0}, \"latency_ns\": {{{}, \"max\": {}}}, \"timer_overhead_ns\": {overhead}, \"resting_orders\": {}, \"state_hash\": \"{final_hash:016x}\", \"cpu\": \"{}\"}}",
            args.seed,
            best.as_secs_f64(),
            throughput,
            quantiles.join(", "),
            histogram.max(),
            engine.len(),
            cpu_model().replace('"', "'"),
        );
        return Ok(());
    }
    println!(
        "synthetic flow  {} commands, seed {}, {} events",
        group(count),
        args.seed,
        group(events_per_pass)
    );
    println!("cpu             {}", cpu_model());
    println!(
        "throughput      {:.2} M commands/s (best of 3 passes, {:.3} s)",
        throughput / 1e6,
        best.as_secs_f64()
    );
    println!("latency (ns)    per command, timer overhead of {overhead} ns subtracted");
    for p in percentiles {
        println!(
            "  p{:<6}       {:>8}",
            p,
            group(histogram.value_at_quantile(p / 100.0))
        );
    }
    println!("  max           {:>8}", group(histogram.max()));
    println!(
        "final book      {} resting orders, state hash {final_hash:016x}",
        group(engine.len() as u64)
    );
    Ok(())
}

/// Median cost of two back-to-back clock reads.
fn timer_overhead() -> u64 {
    let mut samples: Vec<u64> = (0..10_001)
        .map(|_| {
            let started = Instant::now();
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(0)
        })
        .collect();
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split(':').nth(1))
                .map(|name| name.trim().to_owned())
        })
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned())
}

fn rate(count: u64, elapsed: Duration) -> String {
    if elapsed.is_zero() {
        return "n/a".to_owned();
    }
    group((count as f64 / elapsed.as_secs_f64()) as u64)
}

fn group(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn demo() {
    use crossbook::{NewOrder, Side, StpMode, TimeInForce};
    let steps: [(&str, Command); 9] = [
        (
            "Two sellers rest at 101 and one at 100",
            Command::New(NewOrder::limit(1, Side::Sell, 101, 5)),
        ),
        ("", Command::New(NewOrder::limit(2, Side::Sell, 101, 5))),
        ("", Command::New(NewOrder::limit(3, Side::Sell, 100, 5))),
        (
            "A buy for 8 @ 101 takes the better price first, then the older order at 101",
            Command::New(NewOrder::limit(4, Side::Buy, 101, 8)),
        ),
        (
            "Fill-or-kill for 20 cannot fill in full, so it is rejected and nothing changes",
            Command::New(NewOrder::limit(5, Side::Buy, 101, 20).tif(TimeInForce::Fok)),
        ),
        (
            "Post-only would take liquidity, so it is rejected instead of trading",
            Command::New(NewOrder::limit(6, Side::Buy, 101, 1).post_only()),
        ),
        (
            "Order 2's owner buys 3 @ 101 with cancel-newest self-trade prevention: it fills 2 lots from order 1, then stops instead of trading with its own order",
            Command::New(
                NewOrder::limit(7, Side::Buy, 101, 3)
                    .account(9)
                    .stp(StpMode::CancelNewest),
            ),
        ),
        (
            "Shrinking order 2 keeps its place in the queue",
            Command::Modify {
                id: 2,
                price: 101,
                qty: 4,
            },
        ),
        (
            "A market sell for 10 hits an empty bid side and cancels",
            Command::New(NewOrder::market(8, Side::Sell, 10)),
        ),
    ];
    let mut engine = Engine::new();
    let mut events = Vec::new();
    // Order 2 belongs to account 9 so the self-trade step has something to prevent.
    let mut first = true;
    for (note, mut command) in steps {
        if let Command::New(ref mut order) = command
            && order.id == 2
        {
            order.account = 9;
        }
        if !note.is_empty() {
            if !first {
                println!();
            }
            println!("# {note}");
            first = false;
        }
        events.clear();
        engine.process(&command, &mut events);
        for event in &events {
            println!("  {event}");
        }
    }
    println!();
    print_depth(&engine, 3);
}
