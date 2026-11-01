//! `crossbook`: generate, replay, verify and benchmark synthetic order flow.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crossbook::exchange::{self, Symbol};
use crossbook::flow::{Flow, FlowConfig, SplitMix64};
use crossbook::market_data::{BookBuilder, Message, Publisher};
use crossbook::reference::ReferenceEngine;
use crossbook::{Command, Engine, Event, digest, journal, recovery, snapshot};
use hdrhistogram::Histogram;

const USAGE: &str = "\
crossbook: deterministic limit order book and matching engine

USAGE:
    crossbook gen     --out FILE [--orders N] [--seed S] [--book N]
    crossbook replay  FILE [--depth N]
    crossbook verify  [--orders N] [--seed S] [--book N]
    crossbook bench   [--orders N] [--seed S] [--book N] [--json]
    crossbook snapshot FILE --at N --out SNAPSHOT
    crossbook recover FILE [--snapshot SNAPSHOT]
    crossbook feed    [--orders N] [--seed S] [--book N]
    crossbook exchange [--orders N] [--symbols N] [--shards LIST] [--batch N] [--runs N] [--seed S]
    crossbook demo

COMMANDS:
    gen      Write N synthetic commands to a journal file
    replay   Run a journal through the engine and print the result and its fingerprints
    verify   Run synthetic flow through the fast engine and the reference model and
             require identical events and books after every command
    bench    Measure throughput and per-command latency on synthetic flow
    snapshot Replay the first N commands of a journal and save the book to a snapshot
    recover  Rebuild the book from a snapshot and the journal after it, as after a crash;
             a torn final frame is reported and ignored
    feed     Publish market data for synthetic flow, rebuild the book from the feed alone
             and require it to match the engine
    exchange Run synthetic flow for many instruments on one thread and on several shard
             threads, report median throughput, and require the same output digest
    demo     Walk through a short annotated session

Defaults: --orders 1000000, --seed 42, --depth 5, --book 10000 (target resting orders;
verify defaults to 1000 because the reference model scans the whole book on every
command; exchange defaults to 2000 per instrument), --symbols 64, --shards 1,2,3,4,
--batch 4096, --runs 3. Flow is synthetic, not market data.";

struct Args {
    positional: Vec<String>,
    orders: usize,
    seed: u64,
    depth: usize,
    book: Option<usize>,
    out: Option<String>,
    json: bool,
    at: Option<u64>,
    snapshot: Option<String>,
    symbols: u32,
    shards: Vec<usize>,
    batch: usize,
    runs: usize,
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
        at: None,
        snapshot: None,
        symbols: 64,
        shards: vec![1, 2, 3, 4],
        batch: 4096,
        runs: 3,
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
            "--at" => args.at = Some(number(&value("--at")?)?),
            "--snapshot" => args.snapshot = Some(value("--snapshot")?),
            "--symbols" => args.symbols = number(&value("--symbols")?)?,
            "--batch" => args.batch = number(&value("--batch")?)?,
            "--runs" => args.runs = number(&value("--runs")?)?,
            "--shards" => {
                args.shards = value("--shards")?
                    .split(',')
                    .map(number)
                    .collect::<Result<_, _>>()?;
            }
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
        Some("snapshot") => take_snapshot(&args),
        Some("recover") => recover(&args),
        Some("feed") => feed(&args),
        Some("exchange") => run_exchange(&args),
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

fn open_journal(path: &str) -> Result<journal::Reader<BufReader<File>>, String> {
    let file = File::open(path).map_err(|error| format!("{path}: {error}"))?;
    journal::Reader::new(BufReader::new(file)).map_err(|error| format!("{path}: {error}"))
}

fn take_snapshot(args: &Args) -> Result<(), String> {
    let path = args
        .positional
        .get(1)
        .ok_or("snapshot needs a journal FILE")?;
    let at = args.at.ok_or("snapshot needs --at N")?;
    let out = args.out.as_deref().ok_or("snapshot needs --out SNAPSHOT")?;
    let mut engine = Engine::new();
    let mut events = Vec::new();
    let mut applied = 0u64;
    for command in open_journal(path)?.take(usize::try_from(at).unwrap_or(usize::MAX)) {
        let command = command.map_err(|error| format!("{path}: {error}"))?;
        events.clear();
        engine.process(&command, &mut events);
        applied += 1;
    }
    if applied < at {
        return Err(format!("{path} holds only {} commands", group(applied)));
    }
    let bytes = snapshot::encode(&engine, applied);
    std::fs::write(out, &bytes).map_err(|error| format!("{out}: {error}"))?;
    println!(
        "wrote {out}: {} resting orders after {} commands ({} bytes)",
        group(engine.len() as u64),
        group(applied),
        group(bytes.len() as u64)
    );
    println!("state hash      {:016x}", engine.state_hash());
    Ok(())
}

fn recover(args: &Args) -> Result<(), String> {
    let path = args
        .positional
        .get(1)
        .ok_or("recover needs a journal FILE")?;
    let image = match args.snapshot.as_deref() {
        Some(file) => Some(std::fs::read(file).map_err(|error| format!("{file}: {error}"))?),
        None => None,
    };
    let file = File::open(path).map_err(|error| format!("{path}: {error}"))?;
    let mut events = Vec::new();
    let started = Instant::now();
    let recovered = recovery::recover(image.as_deref(), BufReader::new(file), &mut events)
        .map_err(|error| format!("{path}: {error}"))?;
    let elapsed = started.elapsed();
    let mut fingerprint = digest::Fingerprint::new();
    for event in &events {
        fingerprint.event(event);
    }
    match args.snapshot.as_deref() {
        Some(file) => println!(
            "snapshot        {file} ({} commands)",
            group(recovered.snapshot_sequence)
        ),
        None => println!("snapshot        none, replaying from the start"),
    }
    println!(
        "replayed        {} commands",
        group(recovered.sequence - recovered.snapshot_sequence)
    );
    if recovered.torn_bytes > 0 {
        println!(
            "journal         {} bytes intact; ignored a torn final frame of {} bytes",
            group(recovered.journal_len),
            recovered.torn_bytes
        );
    } else {
        println!(
            "journal         {} bytes, clean",
            group(recovered.journal_len)
        );
    }
    println!("resting orders  {}", group(recovered.engine.len() as u64));
    println!("tail digest     {:016x}", fingerprint.value());
    println!("state hash      {:016x}", recovered.engine.state_hash());
    println!("recovery time   {:.3} s", elapsed.as_secs_f64());
    Ok(())
}

fn feed(args: &Args) -> Result<(), String> {
    let commands = synthetic(args, 10_000);
    let mut engine = Engine::new();
    let mut publisher = Publisher::new();
    let mut builder = BookBuilder::new();
    let (mut events, mut messages) = (Vec::new(), Vec::new());
    let (mut prints, mut levels) = (0u64, 0u64);
    let mut fingerprint = digest::Fingerprint::new();
    let mut busy = Duration::ZERO;
    for (index, command) in commands.iter().enumerate() {
        events.clear();
        messages.clear();
        engine.process(command, &mut events);
        let started = Instant::now();
        publisher.publish(&events, &mut messages);
        for message in &messages {
            builder
                .apply(message)
                .map_err(|gap| format!("command {index}: {gap}"))?;
        }
        busy += started.elapsed();
        for message in &messages {
            fingerprint.u64(message.seq());
            match *message {
                Message::Trade {
                    price,
                    qty,
                    aggressor,
                    ..
                } => {
                    prints += 1;
                    fingerprint.u8(0);
                    fingerprint.u8(u8::from(aggressor == crossbook::Side::Sell));
                    fingerprint.i64(price);
                    fingerprint.u64(qty);
                }
                Message::Level {
                    side,
                    price,
                    qty,
                    orders,
                    ..
                } => {
                    levels += 1;
                    fingerprint.u8(1);
                    fingerprint.u8(u8::from(side == crossbook::Side::Sell));
                    fingerprint.i64(price);
                    fingerprint.u64(qty);
                    fingerprint.u32(orders);
                }
            }
        }
        if (index.is_multiple_of(1024) || index + 1 == commands.len())
            && builder.depth(usize::MAX) != engine.depth(usize::MAX)
        {
            return Err(format!(
                "the rebuilt book diverged from the engine at command {index}"
            ));
        }
    }
    println!(
        "ok: the book rebuilt from market data matches the engine after {} commands (seed {})",
        group(commands.len() as u64),
        args.seed
    );
    println!(
        "messages        {} trade prints, {} level updates ({:.2} per command)",
        group(prints),
        group(levels),
        (prints + levels) as f64 / commands.len().max(1) as f64
    );
    println!("feed digest     {:016x}", fingerprint.value());
    println!(
        "feed time       {:.3} s to publish and apply ({} messages/s)",
        busy.as_secs_f64(),
        rate(prints + levels, busy)
    );
    Ok(())
}

fn run_exchange(args: &Args) -> Result<(), String> {
    if args.symbols == 0
        || args.batch == 0
        || args.runs == 0
        || args.shards.is_empty()
        || args.shards.contains(&0)
    {
        return Err(
            "--symbols, --batch, --runs and every --shards value must be positive".to_owned(),
        );
    }
    let book = args.book.unwrap_or(2_000);
    let mut flows: Vec<Flow> = (0..args.symbols)
        .map(|symbol| {
            Flow::new(FlowConfig {
                target_orders: book,
                ..FlowConfig::seeded(args.seed.wrapping_add(u64::from(symbol)))
            })
        })
        .collect();
    let mut pick = SplitMix64::new(args.seed ^ 0x9e37_79b9_7f4a_7c15);
    let commands: Vec<(Symbol, Command)> = (0..args.orders)
        .map(|_| {
            let symbol = pick.below(u64::from(args.symbols)) as Symbol;
            (
                symbol,
                flows[symbol as usize].next().expect("flow is endless"),
            )
        })
        .collect();
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    println!(
        "{} commands across {} instruments of about {} resting orders (seed {}), batches of {}, \
         median of {} runs, {threads} hardware threads",
        group(commands.len() as u64),
        args.symbols,
        group(book as u64),
        args.seed,
        args.batch,
        args.runs
    );
    println!(
        "{:>8}  {:>14}  {:>9}  output digest",
        "threads", "commands/s", "vs inline"
    );

    // Baseline: every book on the calling thread, no pipeline.
    let mut timings = Vec::with_capacity(args.runs);
    let mut digest = 0;
    for run in 0..args.runs {
        let mut books: Vec<Engine> = (0..args.symbols).map(|_| Engine::new()).collect();
        let mut events = Vec::new();
        let mut fingerprint = digest::Fingerprint::new();
        let started = Instant::now();
        for (seq, &(symbol, command)) in (0u64..).zip(&commands) {
            events.clear();
            books[symbol as usize].process(&command, &mut events);
            if run == 0 {
                fingerprint.u64(seq);
                fingerprint.u32(symbol);
                for event in &events {
                    fingerprint.event(event);
                }
            }
        }
        if run == 0 {
            digest = fingerprint.value();
        } else {
            timings.push(started.elapsed());
        }
    }
    if args.runs == 1 {
        // The only run also built the digest, which costs time; time a clean pass.
        let mut books: Vec<Engine> = (0..args.symbols).map(|_| Engine::new()).collect();
        let mut events = Vec::new();
        let started = Instant::now();
        for &(symbol, command) in &commands {
            events.clear();
            books[symbol as usize].process(&command, &mut events);
        }
        timings.push(started.elapsed());
    }
    let inline = commands.len() as f64 / median(&mut timings).as_secs_f64();
    println!(
        "{:>8}  {:>14}  {:>8.2}x  {digest:016x}",
        "inline",
        group(inline as u64),
        1.0
    );

    for &shards in &args.shards {
        let config = exchange::Config {
            shards,
            batch: args.batch,
            queue: 8,
        };
        // Check run: fingerprint every event in delivery order.
        let mut fingerprint = digest::Fingerprint::new();
        exchange::run(commands.iter().copied(), config, |output| {
            fingerprint.u64(output.seq);
            fingerprint.u32(output.symbol);
            for event in output.events {
                fingerprint.event(event);
            }
        });
        if fingerprint.value() != digest {
            return Err(format!(
                "output digest with {shards} shards is {:016x}, not {digest:016x}",
                fingerprint.value()
            ));
        }
        // Timed runs: the sink only touches each output, so they measure the pipeline.
        let mut timings: Vec<Duration> = (0..args.runs)
            .map(|_| {
                let mut touched = 0u64;
                let started = Instant::now();
                exchange::run(commands.iter().copied(), config, |output| {
                    touched = touched.wrapping_add(output.seq ^ output.events.len() as u64);
                });
                std::hint::black_box(touched);
                started.elapsed()
            })
            .collect();
        let throughput = commands.len() as f64 / median(&mut timings).as_secs_f64();
        println!(
            "{:>8}  {:>14}  {:>8.2}x  {digest:016x}",
            format!("{shards} shard{}", if shards == 1 { "" } else { "s" }),
            group(throughput as u64),
            throughput / inline
        );
    }
    println!("ok: every shard count delivered exactly the inline output");
    Ok(())
}

fn median(timings: &mut [Duration]) -> Duration {
    timings.sort_unstable();
    timings[timings.len() / 2]
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
