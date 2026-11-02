# Matching semantics

This is the contract both engines implement: the fast `Engine` and the linear-scan
`ReferenceEngine`. Every rule here has a scenario test in
[`crates/crossbook/tests/semantics.rs`](../crates/crossbook/tests/semantics.rs), and
the differential property tests require both engines to emit identical events for
random command streams.

## Model

- One instrument. Prices are integer ticks (`i64`), quantities are integer lots (`u64`).
- Commands are processed one at a time, in order. Each command appends one or more
  events to the caller's buffer. The order of those events is part of the contract.
- Nothing depends on wall-clock time. Time priority is the order in which orders were
  booked.
- The book is in one of two trading phases: continuous trading, where incoming orders
  match on arrival (everything up to [Trading phases](#trading-phases-and-call-auctions)
  describes it), or an auction call phase, where orders rest until an uncross.

## Commands

| Command | Fields |
| --- | --- |
| `New` | `id`, `account`, `side`, `kind` (`Limit(price)` or `Market`), `qty`, `tif` (`Gtc`, `Ioc`, `Fok`), `post_only`, `stp` |
| `Cancel` | `id` |
| `Modify` | `id`, new `price`, new remaining `qty` |
| `StartCall` | optional `reference` price |
| `Uncross` | none |
| `Resume` | none |

## New orders

### Validation

Checks run in this order. The first failure produces a single `Rejected` event and the
book does not change.

| # | Check | Reason |
| --- | --- | --- |
| 1 | `qty` is 0 or above `MAX_QTY` (4,294,967,295) | `InvalidQuantity` |
| 2 | Limit price is 0 or negative | `InvalidPrice` |
| 3 | Market order with `Gtc` (continuous trading only) | `MarketOrderCannotRest` |
| 4 | Post-only on a market, IOC or FOK order (continuous trading only) | `PostOnlyRequiresGtcLimit` |
| 5 | `id` belongs to a live (resting) order | `DuplicateOrderId` |
| 6 | Post-only order would trade on arrival | `PostOnlyWouldCross` |
| 7 | FOK order cannot fill in full right now | `FillOrKillUnfillable` |

In a call phase, checks 3 and 4 are replaced by one check: an IOC, FOK or post-only order
is rejected with `NeedsContinuousTrading`. Checks 6 and 7 cannot fire there, since only
GTC orders without post-only remain.

Ids only need to be unique among live orders. Once an order has filled or been
canceled, its id may be reused.

### Matching

An accepted order emits `Accepted`, then trades against the opposite side:

- **Price priority.** A buy trades with the lowest ask first; a sell with the highest
  bid first.
- **Time priority.** Within one price, the order booked earliest trades first.
- **Crossing.** A buy with limit `L` may trade with an ask at price `P` when `P <= L`;
  a sell when `P >= L`. A market order crosses every price.
- **Execution price.** Every trade prints at the resting (maker) order's price.
- Each fill emits `Trade { taker, maker, taker_side, price, qty, taker_remaining,
  maker_remaining }`. A maker that reaches zero leaves the book.

### What happens to the remainder

| Order | Unfilled remainder |
| --- | --- |
| GTC limit | `Booked` at its limit price, at the back of that price's queue |
| IOC limit or market | `Canceled { reason: Unfilled }` |
| FOK | Never has one: it was rejected unless it could fill in full |

A fully filled order emits no further event after its last `Trade`.

## Self-trade prevention (STP)

When the next resting order in line belongs to the same `account` as the incoming
order, the incoming order's `stp` mode decides:

| Mode | Resting order | Incoming order |
| --- | --- | --- |
| `Allow` | trades | trades |
| `CancelNewest` | stays | rest of it is canceled; matching stops |
| `CancelOldest` | canceled | keeps matching against the next order |
| `CancelBoth` | canceled | rest of it is canceled; matching stops |

Cancels caused by STP emit `Canceled { reason: SelfTrade }`. With `CancelBoth`, the
resting order's event comes first. STP only acts when an own order is next in line, so
an incoming order can trade with better-priced liquidity from other accounts before
it reaches its own order.

### Fill-or-kill and STP

A FOK order fills only if enough liquidity remains after STP. The check walks the same
queue the matcher would: own orders that `CancelOldest` would cancel do not count as
liquidity, and under `CancelNewest` or `CancelBoth` the walk stops at the first own
order. A rejected FOK order has no side effects, so it never cancels anyone's resting
order. The reference model defines FOK as "run it as IOC on a copy of the book and see
whether it fills completely", and the property tests check the fast engine's walk
against that definition.

## Cancel

- Live id: `Canceled { remaining, reason: Requested }` and the order leaves the book.
- Unknown id: `Rejected { reason: UnknownOrder }`.

## Modify

Validation, in order: unknown id (`UnknownOrder`), quantity 0 or above `MAX_QTY`
(`InvalidQuantity`), price 0 or negative (`InvalidPrice`).

| Change | Result |
| --- | --- |
| Same price, quantity less than or equal to the current open quantity | `Modified { kept_priority: true }`. The order keeps its place in the queue. |
| Any other change (new price, or more quantity) | `Modified { kept_priority: false }`, then the order re-enters as an incoming GTC limit order with the same id, account, STP mode and post-only flag. It may trade, and any remainder is booked at the back of the queue. |

A post-only order whose new price would cross the opposite side is rejected with
`PostOnlyWouldCross` and stays exactly as it was.

## Trading phases and call auctions

Exchanges open and close the trading day with a call auction, and reopen with one after a
halt. During the call, orders collect without matching; at the uncross, one clearing
price is chosen and every crossing order executes at it.

### Phase commands

Phase commands come from the venue's schedule, not from participants, so they are never
rejected: each one has a defined effect in both phases.

| Command | Effect | Events |
| --- | --- | --- |
| `StartCall { reference }` | Enter the call phase, or stay in it. The auction's reference price becomes `reference` if it is given and positive, otherwise the price of the last trade (none if nothing has traded). Resting post-only orders are canceled. | `CallStarted { reference }`, then `Canceled { reason: CallPhase }` for each post-only order in book order |
| `Uncross` | Run the auction described below. The book stays in the call phase. In continuous trading the book never crosses, so nothing executes. | `AuctionTrade` per fill, `Canceled { reason: Unfilled }` per waiting market order, then `Uncrossed` |
| `Resume` | Return to continuous trading and clear the reference price. If the book is crossed or holds market orders, uncross first, so continuous trading never starts from a crossed book. | the uncross events if it ran, then `Resumed` |

### Orders during a call phase

| Order | During a call phase |
| --- | --- |
| GTC limit | `Accepted`, then `Booked` at its limit. It never trades on arrival, even when it crosses the book. |
| Market | Must be GTC: it waits for the uncross (`Accepted`, then `MarketBooked`) ahead of every limit order on its side. What the uncross does not fill is canceled, because a market order never rests in continuous trading. |
| IOC or FOK, limit or market | Rejected: `NeedsContinuousTrading`. Both need immediate matching, which a call does not offer. |
| Post-only | Rejected: `NeedsContinuousTrading`. Every auction trade takes liquidity, so a post-only order cannot take part; resting ones are canceled when the call starts. |
| Cancel | As in continuous trading, for limit and market orders. |
| Modify | The same validation and priority rules, but a re-entered order rests instead of matching. A waiting market order has no price to keep, so a modify always turns it into a GTC limit order at the back of its level. |

Self-trade prevention does not apply in a call phase: nothing matches on arrival, and
the uncross ignores it. Canceling an order after the clearing price was chosen from its
quantity would make the published price and volume wrong.

### The clearing price

For a price `p`, demand `D(p)` is the quantity of market buys plus bids with a limit at or
above `p`; supply `S(p)` is the quantity of market sells plus asks with a limit at or below
`p`. The executable volume at `p` is `min(D(p), S(p))`, and the surplus is
`|D(p) - S(p)|`, on the buy side when demand is larger.

The candidates are the distinct limit prices on the book. Each rule keeps only the
candidates that tie under the rules before it:

1. **Volume.** The highest executable volume. If it is 0, nothing executes: there is no
   clearing price.
2. **Surplus.** The lowest surplus.
3. **Market pressure.** If every remaining candidate has a buy surplus, the highest price;
   if every one has a sell surplus, the lowest.
4. **Reference price.** The candidate closest to the auction's reference price.
5. **Lowest price.** The lowest remaining candidate, which settles an exact tie under rule
   4 and any tie when there is no reference price.

A book with market orders on both sides and no limit orders has no candidate. It clears at
the reference price, or does not trade without one. The rules, their sources and where
they differ from Xetra and Euronext are in [DESIGN.md](DESIGN.md#call-auctions).

### Execution

At clearing price `P` with volume `V`, each side fills `V` lots in priority order: market
orders first (oldest first), then limit orders from the best price to `P`, oldest first
within a price. Orders with a limit worse than `P` are untouched. The side without a
surplus fills completely; on the other side, the last order reached may fill partly and the
surplus stays on the book. Fills are paired in that order: the first unfilled buy trades
with the first unfilled sell, and each `AuctionTrade` reports both orders' open quantity
afterwards. Then every waiting market order's remainder is canceled, buys before sells,
oldest first, and `Uncrossed` reports the price, volume and surplus. Afterwards the best
bid is below the best ask.

`Engine::indicative_uncross` returns the same price, volume and surplus without executing,
which is what exchanges publish during a call.

`cargo run --release -p crossbook-cli -- auction` walks through an opening call and
prints the indicative uncross after each order. After its last order, 13 lots can trade at
101, and the uncross fills them in this order:

```text
# 8 lots can trade at 100 or 101, both with 2 lots left to sell: with sellers in excess, the lower price
  accepted #4 buy 3 @ 101
  booked #4 buy 3 @ 101
  indicative uncross: 8 lots @ 100, surplus 2 sell

# The uncross: every crossing order trades at 101, market orders first, then better prices, then earlier orders
  auction trade 2 @ 101 (buy #5, sell #1)
  auction trade 4 @ 101 (buy #3, sell #1)
  auction trade 1 @ 101 (buy #3, sell #2)
  auction trade 3 @ 101 (buy #4, sell #2)
  auction trade 3 @ 101 (buy #8, sell #6)
  uncrossed at 101: 13 lots, surplus 1 buy
```

## Event reference

| Event | Meaning |
| --- | --- |
| `Accepted { id, side, price, qty }` | A new order passed validation (`price` is `None` for market orders) |
| `Rejected { id, reason }` | A command failed validation and changed nothing |
| `Trade { taker, maker, taker_side, price, qty, taker_remaining, maker_remaining }` | One fill at the maker's price |
| `Booked { id, side, price, qty }` | The remainder now rests on the book |
| `Canceled { id, remaining, reason }` | Open quantity left without trading: `Requested`, `Unfilled`, `SelfTrade` or `CallPhase` |
| `Modified { id, price, qty, kept_priority }` | A resting order changed |
| `MarketBooked { id, side, qty }` | A market order waits for the uncross; it belongs to no price level |
| `CallStarted { reference }` | The call phase started, with this reference price |
| `AuctionTrade { buy, sell, price, qty, buy_remaining, sell_remaining }` | One fill of an uncross, at the clearing price |
| `Uncrossed { price, volume, imbalance_side, imbalance }` | An uncross finished; `price` is `None` when nothing could execute |
| `Resumed` | Continuous trading resumed |

## Not modeled

Stop and iceberg orders, good-till-date expiry, minimum quantities, price bands,
volatility interruptions that start a call automatically, tick sizes other than 1, and
fees. One book holds one instrument; `exchange` runs many books. The engine keeps the book
in memory; durability comes from journaling commands and replaying them.
