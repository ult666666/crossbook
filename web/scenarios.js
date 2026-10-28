// Guided scenarios for the playground. Each one resets the book, runs `setup`, then waits
// for the visitor to run `action`. Accounts: 1 = Ana, 2 = Ben, 3 = Cleo. The expected
// outcomes below match tests in crates/crossbook/tests/semantics.rs.

const sell = (price, qty, account = 1, extra = {}) => ({ op: 'submit', side: 'sell', kind: 'limit', price, qty, account, ...extra });
const buy = (price, qty, account = 2, extra = {}) => ({ op: 'submit', side: 'buy', kind: 'limit', price, qty, account, ...extra });

export const accounts = { 1: 'Ana', 2: 'Ben', 3: 'Cleo' };

export const scenarios = [
  {
    id: 'priority',
    title: 'Price-time priority',
    rule: 'Better prices trade first. At one price, the earlier order trades first.',
    setup: [sell(101, 5, 1), sell(101, 5, 3), sell(100, 5, 1)],
    action: buy(101, 8, 2),
    actionLabel: 'Ben buys 8 @ 101',
    explain:
      'Ben takes all 5 lots at 100 first, the better price, then 3 lots from order #1, the older of the two orders at 101. Order #2 keeps its place behind #1. Every trade prints at the resting price, so Ben pays 100 for the first 5 lots, not his limit of 101.',
  },
  {
    id: 'sweep',
    title: 'Market order sweep',
    rule: 'A market order trades at any price and never rests. What it cannot fill is canceled.',
    setup: [sell(100, 2, 1), sell(101, 3, 3), sell(103, 4, 1)],
    action: { op: 'submit', side: 'buy', kind: 'market', qty: 12, tif: 'ioc', account: 2 },
    actionLabel: 'Ben sends a market buy for 12',
    explain:
      'The order walks up the book: 2 lots at 100, 3 at 101, 4 at 103. The ask side is then empty, so the last 3 lots are canceled as unfilled instead of resting.',
  },
  {
    id: 'fok',
    title: 'Fill-or-kill vs immediate-or-cancel',
    rule: 'FOK fills in full or is rejected with no effect. IOC fills what it can and cancels the rest.',
    setup: [sell(100, 4, 1), sell(101, 4, 3)],
    action: buy(101, 9, 2, { tif: 'fok' }),
    actionLabel: 'Ben sends FOK buy 9 @ 101',
    followUp: buy(101, 9, 2, { tif: 'ioc' }),
    followUpLabel: 'Now send the same order as IOC',
    explain:
      'Only 8 lots are available at 101 or better, so the FOK order is rejected and the book does not change. The same order as IOC buys all 8 lots and cancels the missing 1.',
  },
  {
    id: 'post-only',
    title: 'Post-only',
    rule: 'A post-only order may only add liquidity. If it would trade on arrival, it is rejected.',
    setup: [sell(100, 5, 1)],
    action: buy(100, 5, 2, { postOnly: true }),
    actionLabel: 'Ben sends post-only buy 5 @ 100',
    followUp: buy(99, 5, 2, { postOnly: true }),
    followUpLabel: 'Now post-only buy 5 @ 99',
    explain:
      'At 100 the order would cross the resting ask, so it is rejected instead of trading. At 99 it rests on the bid side.',
  },
  {
    id: 'stp',
    title: 'Self-trade prevention',
    rule: 'An account should not trade with itself. The incoming order chooses what to cancel.',
    setup: [sell(100, 5, 1), sell(101, 5, 2)],
    action: buy(101, 8, 1, { stp: 'cancel-oldest' }),
    actionLabel: 'Ana buys 8 @ 101 (cancel oldest)',
    followUp: sell(101, 2, 1, { stp: 'cancel-newest' }),
    followUpLabel: 'Ana sells 2 @ 101 (cancel newest)',
    explain:
      'Ana’s buy would first hit her own ask at 100. Cancel-oldest removes that resting ask, then the buy trades 5 lots with Ben at 101 and rests the other 3. In the follow-up, Ana’s sell would hit her own resting bid. Cancel-newest cancels the incoming sell and leaves her bid in place.',
  },
  {
    id: 'modify',
    title: 'Modify and queue priority',
    rule: 'Reducing quantity at the same price keeps your place. Any other change sends you to the back.',
    setup: [sell(100, 10, 1), sell(100, 10, 3)],
    action: { op: 'modify', id: 1, price: 100, qty: 4 },
    actionLabel: 'Shrink #1 to 4 lots',
    followUp: { op: 'modify', id: 1, price: 100, qty: 12 },
    followUpLabel: 'Grow #1 to 12 lots',
    explain:
      'Shrinking #1 keeps it at the front of the queue at 100. Growing it to 12 is treated as a new order: #1 moves behind #2.',
  },
];
