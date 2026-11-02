# Changelog

All notable changes to crossbook. The project follows [Semantic Versioning](https://semver.org/);
before 1.0, a minor version may change the public API.

## Unreleased

### Changed

- Playground: section labels, event types and table headings use sentence case instead of
  small capitals, and the footer gives the name, the source link and the license.
- The README is shorter. The test counts and mutation-testing results it listed are in
  `docs/TESTING.md`, and the benchmark details in `docs/BENCHMARKS.md`.

### Fixed

- Playground: at phone width, the table of reference digests was wider than the screen
  until a stress test ran.

- Playground: the first click on the order button right after typing a price or quantity
  could place no order. Leaving the field redrew the button's label under the pointer, and
  a click that started on a number in the label lost its target. The bug dates from 0.1.
- Playground: starting a call phase or resuming continuous trading with the keyboard
  dropped focus to the top of the page, because the button hides itself. Focus now moves
  to the phase control that replaces it.

## 0.3.0 (2026-11-02)

### Added

- **Call auctions.** Each book has a trading phase: continuous trading, or an auction call
  phase in which orders rest without matching. `Command::StartCall` enters the call with
  an optional reference price, `Command::Uncross` executes it at one clearing price, and
  `Command::Resume` returns to continuous trading. The clearing price maximizes executable
  volume, then minimizes the surplus, then follows market pressure, then the reference
  price, then takes the lowest price. All fills happen at that price in price-time
  priority, with market orders first. See `docs/SEMANTICS.md`.
- `Engine::indicative_uncross` returns the price, volume and surplus an uncross would
  produce now, as exchanges publish during a call. Also `Engine::phase`,
  `Engine::reference_price`, `Engine::last_trade_price` and `Engine::market_orders`.
- New events: `CallStarted`, `MarketBooked`, `AuctionTrade`, `Uncrossed` and `Resumed`;
  new reasons `RejectReason::NeedsContinuousTrading` and `CancelReason::CallPhase`; the
  `UncrossSummary` and `Phase` types.
- `reference::auction_price`, a brute-force price determination that tries every
  candidate, and the reference model's own phases and uncross.
- Market data: `Message::Status` for phase changes and `Message::Auction` for each uncross;
  `BookBuilder::phase` and `BookBuilder::last_auction`.
- `flow::Schedule`: synthetic trading days with an opening and a closing auction. The CLI
  takes `--auctions` on `gen`, `verify`, `bench`, `feed` and `exchange`, and `crossbook
  auction` walks through an opening auction.
- The browser playground gets phase controls, the indicative uncross, an opening-auction
  scenario and an auction option for the stress test.
- Tests: brute-force comparison of the uncross with ties at every rule, allocation
  invariants after every uncross, auction sessions in the differential, crash-recovery,
  market-data and shard-independence tests, a crash inside a call phase, and journal and
  snapshot files written by 0.2. CI runs the auction sessions on every push.

### Changed

- The journal format is version 2 (frame types 4 to 6 for the phase commands) and the
  snapshot format is version 2 (phase, reference price, last trade price and market
  orders). Both readers still accept version 1 files; newer versions fail with
  `UnsupportedVersion`.
- `Command`, `Event`, `RejectReason`, `CancelReason`, `market_data::Message`,
  `JournalError` and `SnapshotError` have new variants, and `BookSnapshot` and
  `FlowConfig` have new fields, so exhaustive matches and struct literals need updating.
- `OrderView::price` is 0 for a market order waiting in a call phase; `OrderView::is_market`
  tells them apart.

## 0.2.0 (2026-10-31)

### Added

- Checksummed, versioned snapshots and write-ahead crash recovery that ignores a torn
  final journal frame.
- A market-data publisher (trade prints and price levels derived from events) and a
  subscriber book with sequence numbers, gap detection and resynchronization.
- A sharded multi-instrument runner whose output is identical for any shard count.
- Mutation testing of the new modules with cargo-mutants.

## 0.1.0 (2026-10-28)

### Added

- The order book and matching engine: price-time priority, limit and market orders, GTC,
  IOC and FOK, post-only, cancel, modify and four self-trade prevention modes.
- A linear-scan reference model and differential property tests against it.
- The binary codec and journal, the synthetic flow generator, the CLI, benchmarks, the
  WebAssembly bindings and the browser playground.
