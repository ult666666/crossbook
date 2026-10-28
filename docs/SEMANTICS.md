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

## Commands

| Command | Fields |
| --- | --- |
| `New` | `id`, `account`, `side`, `kind` (`Limit(price)` or `Market`), `qty`, `tif` (`Gtc`, `Ioc`, `Fok`), `post_only`, `stp` |
| `Cancel` | `id` |
| `Modify` | `id`, new `price`, new remaining `qty` |

## New orders

### Validation

Checks run in this order. The first failure produces a single `Rejected` event and the
book does not change.

| # | Check | Reason |
| --- | --- | --- |
| 1 | `qty` is 0 or above `MAX_QTY` (4,294,967,295) | `InvalidQuantity` |
| 2 | Limit price is 0 or negative | `InvalidPrice` |
| 3 | Market order with `Gtc` | `MarketOrderCannotRest` |
| 4 | Post-only on a market, IOC or FOK order | `PostOnlyRequiresGtcLimit` |
| 5 | `id` belongs to a live (resting) order | `DuplicateOrderId` |
| 6 | Post-only order would trade on arrival | `PostOnlyWouldCross` |
| 7 | FOK order cannot fill in full right now | `FillOrKillUnfillable` |

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

## Event reference

| Event | Meaning |
| --- | --- |
| `Accepted { id, side, price, qty }` | A new order passed validation (`price` is `None` for market orders) |
| `Rejected { id, reason }` | A command failed validation and changed nothing |
| `Trade { taker, maker, taker_side, price, qty, taker_remaining, maker_remaining }` | One fill at the maker's price |
| `Booked { id, side, price, qty }` | The remainder now rests on the book |
| `Canceled { id, remaining, reason }` | Open quantity left without trading: `Requested`, `Unfilled` or `SelfTrade` |
| `Modified { id, price, qty, kept_priority }` | A resting order changed |

## Not modeled

Multiple instruments, opening and closing auctions, stop and iceberg orders,
good-till-date expiry, minimum quantities, price bands and circuit breakers, tick
sizes other than 1, fees, and a market-data feed. The engine keeps the book in memory;
durability comes from journaling commands and replaying them.
