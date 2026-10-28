// crossbook playground: drives the WebAssembly engine and renders the order ticket, the
// book, the event log, the guided scenarios and the stress test. No dependencies and no
// build step. The engine is the only source of truth: the page never decides whether an
// order is valid or what it trades with.

import init, { Playground } from './pkg/crossbook_wasm.js';
import { accounts, scenarios } from './scenarios.js';

const LEVELS = 8; // price levels per side shown in the ladder
const LOG_LIMIT = 50; // command groups kept in the event log
const INPUT_MAX = 999_999; // keeps ticket numbers to sane integers
const SEED_MAX = 4_294_967_295; // the engine takes a u32 seed

// Digests pinned by the native Rust tests (x86-64), keyed by `${seed}:${commands}`.
const NATIVE = new Map([
  ['42:100000', { eventDigest: '3749b835b0ff0cae', stateHash: 'ee0bfb94bbfc39a7' }],
  ['42:1000000', { eventDigest: 'f872e8139008e21f', stateHash: 'cd43f17fdee94c04' }],
]);

const TIF_NAME = { gtc: 'GTC', ioc: 'IOC', fok: 'FOK' };
const STP_NAME = {
  'cancel-newest': 'cancel newest',
  'cancel-oldest': 'cancel oldest',
  'cancel-both': 'cancel both',
};

const fmt = new Intl.NumberFormat('en-US');
const byId = (id) => document.getElementById(id);

const ui = {
  ticketForm: byId('ticket-form'),
  price: byId('price'),
  priceStepper: byId('price-stepper'),
  qty: byId('qty'),
  stp: byId('stp'),
  postOnly: byId('post-only'),
  submit: byId('submit'),
  submitLabel: byId('submit-label'),
  ticketResult: byId('ticket-result'),
  ticketHint: byId('ticket-hint'),

  ladder: byId('ladder'),
  asks: byId('asks'),
  bids: byId('bids'),
  spread: byId('spread'),
  bookStatus: byId('book-status'),
  clearBook: byId('clear-book'),

  selEmpty: byId('selection-empty'),
  selBody: byId('selection-body'),
  selSummary: byId('sel-summary'),
  modForm: byId('modify-form'),
  modPrice: byId('mod-price'),
  modQty: byId('mod-qty'),
  modBtn: byId('modify-btn'),
  cancelBtn: byId('cancel-btn'),
  deselect: byId('deselect'),

  scList: byId('scenario-list'),
  scEmpty: byId('scenario-empty'),
  scContent: byId('scenario-content'),
  scTitle: byId('sc-title'),
  scRule: byId('sc-rule'),
  scSetup: byId('sc-setup'),
  scAction: byId('sc-action'),
  scFollow: byId('sc-follow'),
  scRestart: byId('sc-restart'),
  scDirty: byId('sc-dirty'),
  scExplain: byId('sc-explain'),
  scExplainText: byId('sc-explain-text'),
  scWaiting: byId('sc-waiting'),

  logList: byId('log-list'),
  logEmpty: byId('log-empty'),

  stressRun: byId('stress-run'),
  seed: byId('seed'),
  stressOut: byId('stress-out'),
  stressCount: document.querySelector('[data-field="count"]'),

  announcer: byId('announcer'),
};

const state = {
  engine: null,
  orders: new Map(), // id -> what was submitted (account, side, kind, tif, ...)
  ticket: { side: 'buy', kind: 'limit', tif: 'gtc', account: 2 },
  selected: null, // selected resting order id
  modifyFor: null, // order id the modify inputs were last filled for
  book: null, // last book snapshot from the engine
  levelQty: new Map(), // `${side}:${price}` -> qty at the previous render
  lastTrade: null,
  scenario: null, // { def, step: 0 | 1 | 2, dirty, setup }
  stressCount: 100_000,
  stressRunning: false,
};

// ---------------------------------------------------------------------------------------
// Small helpers

// Flattens children and drops empty ones (null, false, ''), so they never render as text.
function nodes(children) {
  const out = [];
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false || child === '') continue;
    out.push(child instanceof Node ? child : String(child));
  }
  return out;
}

function h(tag, attrs, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs ?? {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'class') node.className = value;
    else node.setAttribute(key, value === true ? '' : String(value));
  }
  node.append(...nodes(children));
  return node;
}

function fill(node, ...children) {
  node.replaceChildren(...nodes(children));
}

function svgIcon(path) {
  const ns = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(ns, 'svg');
  svg.setAttribute('viewBox', '0 0 16 16');
  svg.setAttribute('class', 'icon');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  const p = document.createElementNS(ns, 'path');
  p.setAttribute('d', path);
  svg.append(p);
  return svg;
}

const CHECK = 'M3.5 8.5l3 3 6-7';

// Sentences are arrays of plain strings and { n } tokens; tokens render in the mono face.
const n = (value) => ({ n: String(value) });
const ref = (id) => n(`#${id}`);
const q = (qty) => n(fmt.format(qty));

function lots(qty) {
  return [q(qty), qty === 1 ? ' lot' : ' lots'];
}

function plural(count, word) {
  return `${fmt.format(count)} ${word}${count === 1 ? '' : 's'}`;
}

function ordinal(k) {
  const tens = k % 100;
  if (tens >= 11 && tens <= 13) return `${k}th`;
  return `${k}${{ 1: 'st', 2: 'nd', 3: 'rd' }[k % 10] ?? 'th'}`;
}

function toText(parts) {
  return parts.map((part) => (typeof part === 'string' ? part : part.n)).join('');
}

function toNodes(parts) {
  return parts.map((part) => (typeof part === 'string' ? part : h('span', { class: 'num' }, part.n)));
}

function accountName(account) {
  return accounts[account] ?? `Account ${account}`;
}

function ownerName(id) {
  const order = state.orders.get(id);
  return order ? accountName(order.account) : null;
}

function readInt(input, max) {
  const value = Math.trunc(Number(input.value));
  if (!Number.isFinite(value)) return 0;
  return Math.min(Math.max(value, 0), max);
}

function announce(text) {
  ui.announcer.textContent = '';
  window.setTimeout(() => {
    ui.announcer.textContent = text;
  }, 60);
}

function nextPaint() {
  return new Promise((resolve) => {
    requestAnimationFrame(() => window.setTimeout(resolve, 0));
  });
}

// ---------------------------------------------------------------------------------------
// Engine calls

function normalize(command) {
  if (command.op !== 'submit') return { ...command };
  const kind = command.kind ?? 'limit';
  return {
    op: 'submit',
    side: command.side,
    kind,
    price: kind === 'market' ? 0 : command.price,
    qty: command.qty,
    tif: command.tif ?? 'gtc',
    postOnly: Boolean(command.postOnly),
    account: command.account,
    stp: command.stp ?? 'allow',
  };
}

function run(command) {
  const engine = state.engine;
  let id;
  let json;
  if (command.op === 'submit') {
    id = engine.nextId();
    state.orders.set(id, { ...command });
    json = engine.submit(
      command.side,
      command.kind,
      command.price,
      command.qty,
      command.tif,
      command.postOnly,
      command.account,
      command.stp,
    );
  } else if (command.op === 'cancel') {
    id = command.id;
    json = engine.cancel(id);
  } else {
    id = command.id;
    json = engine.modify(id, command.price, command.qty);
  }
  const events = JSON.parse(json);
  for (const event of events) {
    if (event.type === 'trade') state.lastTrade = { price: event.price, qty: event.qty };
  }
  return { command, id, events };
}

function resetEngine() {
  state.engine.reset();
  ui.ticketResult.hidden = true;
  state.orders.clear();
  state.selected = null;
  state.modifyFor = null;
  state.lastTrade = null;
  state.levelQty = new Map();
}

function touchedIds(events) {
  const ids = new Set();
  for (const event of events) {
    if (event.type === 'trade') {
      ids.add(event.maker);
      ids.add(event.taker);
    } else if (event.type === 'booked' || event.type === 'modified') {
      ids.add(event.id);
    }
  }
  return ids;
}

// ---------------------------------------------------------------------------------------
// Plain-English descriptions

function commandParts(command, { short = false } = {}) {
  if (command.op === 'submit') {
    const verb = command.side === 'buy' ? 'buys' : 'sells';
    const what = command.kind === 'market' ? [q(command.qty), ' at market'] : [q(command.qty), ' @ ', n(command.price)];
    const quals = [TIF_NAME[command.tif]];
    if (command.postOnly) quals.push('post-only');
    if (command.stp !== 'allow') quals.push(`STP ${STP_NAME[command.stp]}`);
    const showQuals = !(short && quals.length === 1 && command.tif === 'gtc');
    return [`${accountName(command.account)} ${verb} `, ...what, showQuals ? ` (${quals.join(', ')})` : ''];
  }
  const owner = ownerName(command.id);
  if (command.op === 'cancel') {
    return owner ? [`${owner} cancels `, ref(command.id)] : ['Cancel ', ref(command.id)];
  }
  return [owner ? `${owner} modifies ` : 'Modify ', ref(command.id), ' to ', q(command.qty), ' @ ', n(command.price)];
}

function rejectReason(reason, command) {
  switch (reason) {
    case 'invalid-quantity':
      return ['the quantity must be at least 1 lot.'];
    case 'invalid-price':
      return ['a limit price must be at least 1.'];
    case 'market-order-cannot-rest':
      return ['a market order never rests, so it cannot be GTC. Use IOC or FOK.'];
    case 'post-only-requires-gtc-limit':
      return ['post-only works only on GTC limit orders.'];
    case 'duplicate-order-id':
      return ['another live order already uses this id.'];
    case 'unknown-order':
      return ['no resting order has this id.'];
    case 'post-only-would-cross':
      return command.op === 'modify'
        ? ['at the new price it would trade, and post-only orders only add liquidity.']
        : ['it would trade on arrival, and post-only orders only add liquidity.'];
    case 'fill-or-kill-unfillable':
      return ['the book cannot fill all ', ...lots(command.qty), ' right now, so nothing trades.'];
    default:
      return [`${reason}.`];
  }
}

function eventParts(event, result) {
  const { command } = result;
  switch (event.type) {
    case 'accepted': {
      const head = ['Order ', ref(event.id), ` accepted: ${event.side} `, q(event.qty)];
      return event.price === null ? [...head, ' at market.'] : [...head, ', limit ', n(event.price), '.'];
    }
    case 'trade': {
      const bought = event.side === 'buy';
      const maker = ownerName(event.maker);
      return [
        ref(event.taker),
        bought ? ' bought ' : ' sold ',
        ...lots(event.qty),
        ' @ ',
        n(event.price),
        bought ? ' from ' : ' to ',
        ref(event.maker),
        maker ? ` (${maker})` : '',
        '. ',
        ...(event.makerRemaining === 0
          ? [ref(event.maker), ' is fully filled; ']
          : [ref(event.maker), ' keeps ', q(event.makerRemaining), ' on the book; ']),
        ...(event.takerRemaining === 0
          ? [ref(event.taker), ' is fully filled.']
          : [ref(event.taker), ' still needs ', q(event.takerRemaining), '.']),
      ];
    }
    case 'booked':
      return [
        ref(event.id),
        ` rests on the ${event.side === 'buy' ? 'bid' : 'ask'} side: `,
        ...lots(event.qty),
        ' @ ',
        n(event.price),
        '.',
      ];
    case 'canceled': {
      if (event.reason === 'requested') {
        return [
          ref(event.id),
          ' canceled on request. ',
          ...lots(event.remaining),
          event.remaining === 1 ? ' leaves' : ' leave',
          ' the book.',
        ];
      }
      if (event.reason === 'unfilled') {
        const order = state.orders.get(event.id);
        const buys = order?.side !== 'sell';
        const market = order?.kind === 'market';
        const left = market
          ? `no ${buys ? 'asks' : 'bids'} are left`
          : `no ${buys ? 'asks' : 'bids'} are left at ${order?.price} or ${buys ? 'less' : 'more'}`;
        return [
          ref(event.id),
          ' cancels its remaining ',
          ...lots(event.remaining),
          `: ${left}, and ${market ? 'market' : 'IOC'} orders never rest.`,
        ];
      }
      if (event.id === result.id) {
        return [
          'Self-trade prevention cancels incoming ',
          ref(event.id),
          ' (',
          ...lots(event.remaining),
          '), so it does not trade with its own account.',
        ];
      }
      const owner = ownerName(event.id);
      return [
        'Self-trade prevention cancels resting ',
        ref(event.id),
        ' (',
        owner ? `${owner}, ` : '',
        ...lots(event.remaining),
        '): it belongs to the same account as the incoming order.',
      ];
    }
    case 'rejected': {
      const subject =
        command.op === 'submit'
          ? ['Order ', ref(event.id)]
          : [command.op === 'cancel' ? 'Cancel of ' : 'Modify of ', ref(event.id)];
      const after = command.op === 'modify' ? ' The order stays as it was.' : ' The book does not change.';
      return [...subject, ' rejected: ', ...rejectReason(event.reason, command), after];
    }
    case 'modified':
      return event.keptPriority
        ? [ref(event.id), ' is now ', ...lots(event.qty), ' @ ', n(event.price), ' and keeps its place in the queue.']
        : [
            ref(event.id),
            ' is now ',
            ...lots(event.qty),
            ' @ ',
            n(event.price),
            '. It loses its queue place and re-enters like a new order.',
          ];
    default:
      return [JSON.stringify(event)];
  }
}

// One line on what a command did, e.g. "2 trades for 8 lots, 3 lots rest at 101".
function outcome(result) {
  const rejected = result.events.find((event) => event.type === 'rejected');
  if (rejected) return { rejected: true, text: `rejected: ${toText(rejectReason(rejected.reason, result.command))}` };
  const bits = [];
  const trades = result.events.filter((event) => event.type === 'trade');
  if (trades.length) {
    const filled = trades.reduce((sum, event) => sum + event.qty, 0);
    bits.push(`${plural(trades.length, 'trade')} for ${plural(filled, 'lot')}`);
  }
  for (const event of result.events) {
    if (event.type === 'booked')
      bits.push(`${plural(event.qty, 'lot')} ${event.qty === 1 ? 'rests' : 'rest'} at ${event.price}`);
    if (event.type === 'canceled' && event.reason === 'unfilled')
      bits.push(`${plural(event.remaining, 'lot')} canceled unfilled`);
    if (event.type === 'canceled' && event.reason === 'self-trade')
      bits.push(`self-trade prevention cancels #${event.id}`);
    if (event.type === 'canceled' && event.reason === 'requested') bits.push(`#${event.id} canceled`);
    if (event.type === 'modified')
      bits.push(
        event.keptPriority ? `#${event.id} keeps its queue place` : `#${event.id} goes to the back of the queue`,
      );
  }
  if (!bits.length) bits.push('accepted, nothing traded');
  return { rejected: false, text: `${bits.join(', ')}.` };
}

function summaryText(result) {
  return `${toText(commandParts(result.command))}: ${outcome(result).text}`;
}

// ---------------------------------------------------------------------------------------
// Event log

const TAG = {
  accepted: 'Accepted',
  trade: 'Trade',
  booked: 'Booked',
  canceled: 'Canceled',
  rejected: 'Rejected',
  modified: 'Modified',
};

function eventList(result) {
  return h(
    'ol',
    { class: 'events' },
    result.events.map((event) => {
      const side = event.type === 'booked' ? (event.side === 'buy' ? ' is-bid' : ' is-ask') : '';
      return h(
        'li',
        { class: 'event', 'data-type': event.type },
        h('span', { class: `tag tag-${event.type}${side}` }, TAG[event.type] ?? event.type),
        h('span', { class: 'event-text' }, toNodes(eventParts(event, result))),
      );
    }),
  );
}

function pushGroup(group) {
  ui.logList.querySelector('.is-latest')?.classList.remove('is-latest');
  group.classList.add('is-latest');
  ui.logList.prepend(group);
  while (ui.logList.children.length > LOG_LIMIT) ui.logList.lastElementChild.remove();
  ui.logEmpty.hidden = true;
  ui.logList.parentElement.scrollTop = 0;
}

function logCommand(result) {
  const meta = result.command.op === 'submit' ? h('span', { class: 'group-meta num' }, `#${result.id}`) : null;
  pushGroup(
    h(
      'li',
      { class: 'group', 'data-kind': 'command' },
      h('div', { class: 'group-head' }, h('p', { class: 'group-title' }, toNodes(commandParts(result.command))), meta),
      eventList(result),
    ),
  );
}

function logSetup(title, results) {
  pushGroup(
    h(
      'li',
      { class: 'group group-setup', 'data-kind': 'setup' },
      h(
        'div',
        { class: 'group-head' },
        h('p', { class: 'group-title' }, `Setup: ${title}`),
        h('span', { class: 'group-meta' }, plural(results.length, 'order')),
      ),
      h('p', { class: 'group-note' }, 'Book reset. Ids restart at 1.'),
      results.map((result) =>
        h(
          'div',
          { class: 'subcmd' },
          h('p', { class: 'subcmd-title' }, toNodes(commandParts(result.command))),
          eventList(result),
        ),
      ),
    ),
  );
}

function logNote(title, note) {
  pushGroup(
    h(
      'li',
      { class: 'group', 'data-kind': 'note' },
      h('div', { class: 'group-head' }, h('p', { class: 'group-title' }, title)),
      h('p', { class: 'group-note' }, note),
    ),
  );
}

// ---------------------------------------------------------------------------------------
// Order book ladder

function chipNode(order, index, total, touched) {
  const name = accountName(order.account);
  const label = `#${order.id} ${name}, ${plural(order.qty, 'lot')}, ${ordinal(index + 1)} of ${total} in the queue${order.postOnly ? ', post-only' : ''}`;
  return h(
    'button',
    {
      type: 'button',
      class: touched.has(order.id) ? 'chip is-touched' : 'chip',
      'data-id': order.id,
      'data-key': `chip:${order.id}`,
      'aria-pressed': state.selected === order.id ? 'true' : 'false',
      'aria-label': label,
    },
    h('span', { class: 'chip-id' }, `#${order.id}`),
    h('span', { class: 'chip-acct' }, name.charAt(0)),
    h('span', { class: 'chip-qty' }, fmt.format(order.qty)),
    order.postOnly ? h('span', { class: 'chip-po' }, 'PO') : null,
  );
}

function levelNode(side, level, maxQty, { flash, touched }) {
  const sideName = side === 'buy' ? 'Bid' : 'Ask';
  const key = `${side}:${level.price}`;
  const changed = flash && state.levelQty.get(key) !== level.qty;
  const bar = h('span', { class: 'level-bar' });
  bar.style.setProperty('--w', (level.qty / maxQty).toFixed(4));
  return h(
    'li',
    { class: side === 'buy' ? 'level is-bid' : 'level is-ask' },
    h(
      'button',
      {
        type: 'button',
        class: 'level-btn',
        'data-price': level.price,
        'data-key': `level:${key}`,
        'aria-label': `${sideName} ${level.price}, ${plural(level.qty, 'lot')} in ${plural(level.queue.length, 'order')}`,
        title: `Use ${level.price} as the ticket price`,
      },
      h('span', { class: 'side-tag' }, sideName),
      h('span', { class: 'level-price num' }, level.price),
      h(
        'span',
        { class: changed ? 'level-qty is-changed' : 'level-qty' },
        bar,
        h('span', { class: 'num' }, fmt.format(level.qty)),
      ),
    ),
    h(
      'div',
      { class: 'queue' },
      level.queue.map((order, index) => chipNode(order, index, level.queue.length, touched)),
    ),
  );
}

// One side of the ladder, padded with empty slots to LEVELS rows so the spread band stays
// in place on wide screens (the slots are hidden on narrow ones). Asks list the best price
// last, next to the band; bids list it first.
function sideRows(side, levels, maxQty, options) {
  const ordered = side === 'sell' ? [...levels].reverse() : levels;
  const rows = ordered.map((level) => levelNode(side, level, maxQty, options));
  const filled = rows.length ? rows : [h('li', { class: 'side-empty' }, side === 'buy' ? 'No bids' : 'No asks')];
  const pad = Array.from({ length: Math.max(0, LEVELS - filled.length) }, () =>
    h('li', { class: 'slot', 'aria-hidden': 'true' }),
  );
  return side === 'sell' ? [...pad, ...filled] : [...filled, ...pad];
}

function renderBook({ touched = new Set(), flash = false, traded = false } = {}) {
  const book = JSON.parse(state.engine.book(LEVELS));
  state.book = book;

  const active = document.activeElement;
  const focusKey = active && ui.ladder.contains(active) ? active.dataset.key : null;

  const levels = [...book.asks, ...book.bids];
  const maxQty = Math.max(1, ...levels.map((level) => level.qty));
  const options = { flash, touched };

  fill(ui.asks, sideRows('sell', book.asks, maxQty, options));
  fill(ui.bids, sideRows('buy', book.bids, maxQty, options));

  state.levelQty = new Map([
    ...book.asks.map((level) => [`sell:${level.price}`, level.qty]),
    ...book.bids.map((level) => [`buy:${level.price}`, level.qty]),
  ]);

  renderSpread(book, traded);
  renderBookStatus(book);

  if (focusKey) {
    const target = ui.ladder.querySelector(`[data-key="${CSS.escape(focusKey)}"]`);
    if (target) target.focus({ preventScroll: true });
    else ui.ladder.focus({ preventScroll: true });
  }
}

function renderSpread(book, traded) {
  const bestBid = book.bids[0];
  const bestAsk = book.asks[0];
  let main;
  if (!bestBid && !bestAsk) {
    main = h('span', { class: 'spread-item spread-muted' }, 'Empty book');
  } else {
    main = h(
      'span',
      { class: 'spread-item' },
      h('span', { class: 'spread-label' }, 'Spread'),
      bestBid && bestAsk
        ? h('span', { class: 'num' }, bestAsk.price - bestBid.price)
        : h('span', { class: 'num spread-muted', 'aria-label': 'none, one side is empty' }, '—'),
    );
  }
  const last = state.lastTrade
    ? h(
        'span',
        { class: 'spread-item spread-last' },
        h('span', { class: 'spread-label' }, 'Last trade'),
        h('span', { class: 'num' }, `${fmt.format(state.lastTrade.qty)} @ ${state.lastTrade.price}`),
      )
    : h('span', { class: 'spread-item spread-muted' }, 'No trades yet');
  ui.spread.classList.toggle('is-traded', false);
  fill(ui.spread, main, last);
  if (traded && state.lastTrade) {
    void ui.spread.offsetWidth; // restart the flash
    ui.spread.classList.add('is-traded');
  }
}

function renderBookStatus(book) {
  const hidden = Math.max(0, book.bidLevels - book.bids.length) + Math.max(0, book.askLevels - book.asks.length);
  fill(
    ui.bookStatus,
    h(
      'span',
      { class: 'foot-stats' },
      h(
        'span',
        {},
        h('span', { class: 'num' }, fmt.format(book.orders)),
        book.orders === 1 ? ' resting order' : ' resting orders',
      ),
      hidden ? h('span', {}, `${plural(hidden, 'deeper level')} not shown`) : null,
      h(
        'span',
        { title: 'A fingerprint of every resting order. Equal books give equal hashes.' },
        'State hash ',
        h('span', { class: 'num' }, book.stateHash),
      ),
    ),
    h('span', { class: 'foot-legend' }, 'Chip: id, account, lots. PO: post-only.'),
  );
}

function findOrder(id) {
  if (!state.book) return null;
  for (const [side, levels] of [
    ['sell', state.book.asks],
    ['buy', state.book.bids],
  ]) {
    for (const level of levels) {
      const index = level.queue.findIndex((order) => order.id === id);
      if (index >= 0) {
        return {
          ...level.queue[index],
          side,
          price: level.price,
          position: index + 1,
          queueLength: level.queue.length,
        };
      }
    }
  }
  return null;
}

function updateChipPressed() {
  for (const chip of ui.ladder.querySelectorAll('.chip')) {
    chip.setAttribute('aria-pressed', Number(chip.dataset.id) === state.selected ? 'true' : 'false');
  }
}

// ---------------------------------------------------------------------------------------
// Selection: cancel and modify

// The book snapshot lists only the best LEVELS levels per side, so an order that is not in
// it has either left the book (the last command's events say so) or sits deeper.
function leftTheBook(id, events) {
  return events.some(
    (event) =>
      (event.type === 'canceled' && event.id === id) ||
      (event.type === 'trade' && event.maker === id && event.makerRemaining === 0) ||
      (event.type === 'trade' && event.taker === id && event.takerRemaining === 0),
  );
}

function renderSelection({ moveFocus = false, events = [] } = {}) {
  const id = state.selected;
  const order = id === null ? null : findOrder(id);
  const hadFocus = ui.selBody.contains(document.activeElement);

  if (!order) {
    state.selected = null;
    state.modifyFor = null;
    ui.selBody.hidden = true;
    ui.deselect.hidden = true;
    ui.selEmpty.hidden = false;
    if (id === null) ui.selEmpty.textContent = 'Select an order in the book to cancel or modify it.';
    else if (leftTheBook(id, events)) ui.selEmpty.textContent = `Order #${id} left the book. Select another order.`;
    else ui.selEmpty.textContent = `Order #${id} is now deeper than the ${LEVELS} levels shown. Select another order.`;
    if (hadFocus || moveFocus) ui.selEmpty.focus();
    return;
  }

  ui.selEmpty.hidden = true;
  ui.selBody.hidden = false;
  ui.deselect.hidden = false;
  const sideWord = order.side === 'buy' ? 'Buy' : 'Sell';
  fill(
    ui.selSummary,
    h('span', { class: 'sel-id num' }, `#${order.id}`),
    h(
      'span',
      { class: 'sel-text' },
      `${accountName(order.account)} · `,
      h('span', { class: order.side === 'buy' ? 'is-bid-text' : 'is-ask-text' }, sideWord),
      ' ',
      h('span', { class: 'num' }, fmt.format(order.qty)),
      ' @ ',
      h('span', { class: 'num' }, order.price),
      ` · ${ordinal(order.position)} of ${order.queueLength} in the queue`,
      order.postOnly ? ' · post-only' : '',
    ),
  );
  if (state.modifyFor !== order.id) {
    ui.modPrice.value = order.price;
    ui.modQty.value = order.qty;
    state.modifyFor = order.id;
  }
  ui.modBtn.textContent = `Modify #${order.id}`;
  ui.cancelBtn.textContent = `Cancel #${order.id}`;
}

function select(id) {
  state.selected = state.selected === id ? null : id;
  updateChipPressed();
  renderSelection();
}

// ---------------------------------------------------------------------------------------
// Running commands

function execute(command, { fromScenario = false } = {}) {
  const result = run(command);
  if (!fromScenario && state.scenario) state.scenario.dirty = true;
  if (command.op === 'modify' && command.id === state.selected) state.modifyFor = null;
  logCommand(result);
  renderBook({
    touched: touchedIds(result.events),
    flash: true,
    traded: result.events.some((event) => event.type === 'trade'),
  });
  renderSelection({ events: result.events });
  renderScenario();
  announce(summaryText(result));
  return result;
}

// ---------------------------------------------------------------------------------------
// Order ticket

function ticketOrder() {
  const t = state.ticket;
  return {
    op: 'submit',
    side: t.side,
    kind: t.kind,
    price: t.kind === 'market' ? 0 : readInt(ui.price, INPUT_MAX),
    qty: readInt(ui.qty, INPUT_MAX),
    tif: t.tif,
    postOnly: ui.postOnly.checked,
    account: t.account,
    stp: ui.stp.value,
  };
}

function ticketHint(order) {
  const buys = order.side === 'buy';
  const verb = buys ? 'Buys' : 'Sells';
  const qty = fmt.format(order.qty);
  if (order.kind === 'market') {
    if (order.tif === 'gtc') return 'Market orders never rest, so the engine expects IOC or FOK.';
    if (order.tif === 'fok') return `${verb} all ${qty} at any price right away, or nothing.`;
    return `${verb} at any price. Any remainder is canceled.`;
  }
  const limit = `${order.price} or ${buys ? 'less' : 'more'}`;
  if (order.postOnly) {
    if (order.tif !== 'gtc') return 'Post-only orders only rest, so the engine expects GTC.';
    return `Rests at ${order.price} if it would not trade on arrival. Otherwise the engine rejects it.`;
  }
  if (order.tif === 'fok') return `${verb} all ${qty} at ${limit} right away, or nothing.`;
  if (order.tif === 'ioc') return `${verb} at ${limit}. Any remainder is canceled.`;
  return `${verb} at ${limit}. Any remainder rests on the book.`;
}

function renderTicket() {
  for (const group of ui.ticketForm.querySelectorAll('[data-field]')) {
    const value = String(state.ticket[group.dataset.field]);
    for (const button of group.querySelectorAll('button')) {
      button.setAttribute('aria-pressed', button.dataset.value === value ? 'true' : 'false');
    }
  }
  const order = ticketOrder();
  const market = order.kind === 'market';
  ui.price.disabled = market;
  for (const step of ui.priceStepper.querySelectorAll('.step')) step.disabled = market;
  ui.priceStepper.classList.toggle('is-disabled', market);

  ui.submit.classList.toggle('is-buy', order.side === 'buy');
  ui.submit.classList.toggle('is-sell', order.side === 'sell');
  const quals = [];
  if (order.tif !== 'gtc') quals.push(TIF_NAME[order.tif]);
  if (order.postOnly) quals.push('post-only');
  fill(
    ui.submitLabel,
    order.side === 'buy' ? 'Buy ' : 'Sell ',
    h('span', { class: 'num' }, fmt.format(order.qty)),
    market ? ' at market' : [' @ ', h('span', { class: 'num' }, order.price)],
    quals.length ? h('span', { class: 'submit-quals' }, ` · ${quals.join(' · ')}`) : null,
  );
  ui.ticketHint.textContent = ticketHint(order);
}

function showTicketResult(result) {
  const { rejected, text } = outcome(result);
  ui.ticketResult.className = rejected ? 'ticket-result is-rejected' : 'ticket-result';
  fill(ui.ticketResult, 'Order ', h('span', { class: 'num' }, `#${result.id}`), rejected ? ` ${text}` : `: ${text}`);
  ui.ticketResult.hidden = false;
}

function usePrice(price) {
  ui.price.value = price;
  if (state.ticket.kind === 'market') state.ticket.kind = 'limit';
  renderTicket();
  ui.priceStepper.classList.remove('is-flashed');
  void ui.priceStepper.offsetWidth;
  ui.priceStepper.classList.add('is-flashed');
  announce(`Ticket price set to ${price}.`);
}

ui.ticketForm.addEventListener('click', (event) => {
  const button = event.target.closest('[data-field] button');
  if (!button) return;
  const field = button.parentElement.dataset.field;
  state.ticket[field] = field === 'account' ? Number(button.dataset.value) : button.dataset.value;
  renderTicket();
});

ui.priceStepper.addEventListener('click', (event) => {
  const step = event.target.closest('.step');
  if (!step || step.disabled) return;
  ui.price.value = Math.min(Math.max(readInt(ui.price, INPUT_MAX) + Number(step.dataset.step), 0), INPUT_MAX);
  renderTicket();
});

for (const input of [ui.price, ui.qty]) {
  input.addEventListener('input', renderTicket);
  input.addEventListener('change', () => {
    input.value = readInt(input, INPUT_MAX);
    renderTicket();
  });
}
ui.postOnly.addEventListener('change', renderTicket);
ui.stp.addEventListener('change', renderTicket);

ui.ticketForm.addEventListener('submit', (event) => {
  event.preventDefault();
  if (!state.engine) return;
  ui.price.value = readInt(ui.price, INPUT_MAX);
  ui.qty.value = readInt(ui.qty, INPUT_MAX);
  renderTicket();
  showTicketResult(execute(ticketOrder()));
});

// ---------------------------------------------------------------------------------------
// Ladder and selection events

ui.ladder.addEventListener('click', (event) => {
  const chip = event.target.closest('.chip');
  if (chip) {
    select(Number(chip.dataset.id));
    return;
  }
  const level = event.target.closest('.level-btn');
  if (level) usePrice(Number(level.dataset.price));
});

ui.cancelBtn.addEventListener('click', () => {
  if (state.selected === null) return;
  execute({ op: 'cancel', id: state.selected });
});

ui.modForm.addEventListener('submit', (event) => {
  event.preventDefault();
  if (state.selected === null) return;
  const price = readInt(ui.modPrice, INPUT_MAX);
  const qty = readInt(ui.modQty, INPUT_MAX);
  ui.modPrice.value = price;
  ui.modQty.value = qty;
  execute({ op: 'modify', id: state.selected, price, qty });
});

ui.deselect.addEventListener('click', () => {
  state.selected = null;
  updateChipPressed();
  renderSelection({ moveFocus: true });
});

document.addEventListener('keydown', (event) => {
  if (event.key !== 'Escape' || state.selected === null) return;
  const inside = event.target.closest?.('.book, .selection');
  if (!inside) return;
  state.selected = null;
  updateChipPressed();
  renderSelection();
});

ui.clearBook.addEventListener('click', () => {
  resetEngine();
  state.scenario = null;
  logNote('Book cleared', 'Ids restart at 1.');
  renderBook();
  renderSelection();
  renderScenario();
  announce('Book cleared. Ids restart at 1.');
});

// ---------------------------------------------------------------------------------------
// Guided scenarios

function renderScenarioList() {
  fill(
    ui.scList,
    ...scenarios.map((scenario) =>
      h(
        'li',
        {},
        h(
          'button',
          {
            type: 'button',
            'data-id': scenario.id,
            'aria-current': state.scenario?.def.id === scenario.id ? 'true' : null,
          },
          scenario.title,
        ),
      ),
    ),
  );
}

function updateScenarioList() {
  for (const button of ui.scList.querySelectorAll('button')) {
    if (state.scenario?.def.id === button.dataset.id) button.setAttribute('aria-current', 'true');
    else button.removeAttribute('aria-current');
  }
}

function setStepButton(button, label, { done, locked }) {
  fill(button, ...(done ? [svgIcon(CHECK)] : []), label);
  button.classList.toggle('is-done', done);
  button.setAttribute('aria-disabled', done || locked ? 'true' : 'false');
}

function renderScenario() {
  updateScenarioList();
  const scenario = state.scenario;
  ui.scEmpty.hidden = Boolean(scenario);
  ui.scContent.hidden = !scenario;
  if (!scenario) return;
  const { def, step } = scenario;
  ui.scTitle.textContent = def.title;
  ui.scRule.textContent = def.rule;
  fill(
    ui.scSetup,
    ...scenario.setup.map((result) =>
      h(
        'li',
        {},
        h('span', { class: 'num sc-id' }, `#${result.id}`),
        toNodes(commandParts(result.command, { short: true })),
      ),
    ),
  );
  setStepButton(ui.scAction, def.actionLabel, { done: step >= 1, locked: false });
  ui.scFollow.hidden = !def.followUp;
  if (def.followUp) setStepButton(ui.scFollow, def.followUpLabel, { done: step >= 2, locked: step < 1 });
  ui.scExplainText.textContent = def.explain;
  ui.scExplain.hidden = step < 1;
  ui.scWaiting.hidden = step >= 1;
  ui.scWaiting.textContent = 'Run the step to see what happened.';
  ui.scDirty.hidden = !scenario.dirty;
}

function loadScenario(def, { quiet = false } = {}) {
  resetEngine();
  const setup = def.setup.map((command) => run(normalize(command)));
  state.lastTrade = null;
  state.scenario = { def, step: 0, dirty: false, setup };
  logSetup(def.title, setup);
  renderBook();
  renderSelection();
  renderScenario();
  if (!quiet) announce(`${def.title} loaded with ${plural(setup.length, 'starting order')}. ${def.rule}`);
}

function runScenarioStep(which) {
  const scenario = state.scenario;
  if (!scenario) return;
  if (which === 'action' && scenario.step !== 0) return;
  if (which === 'followUp' && scenario.step !== 1) return;
  scenario.step = which === 'action' ? 1 : 2;
  execute(normalize(scenario.def[which]), { fromScenario: true });
}

ui.scList.addEventListener('click', (event) => {
  const button = event.target.closest('button[data-id]');
  if (!button || !state.engine) return;
  const def = scenarios.find((scenario) => scenario.id === button.dataset.id);
  if (def) loadScenario(def);
});

ui.scAction.addEventListener('click', () => runScenarioStep('action'));
ui.scFollow.addEventListener('click', () => runScenarioStep('followUp'));
ui.scRestart.addEventListener('click', () => {
  if (state.scenario) loadScenario(state.scenario.def);
});

// ---------------------------------------------------------------------------------------
// Stress test

function formatMs(ms) {
  if (ms >= 1000) return `${(ms / 1000).toFixed(2)} s`;
  if (ms >= 100) return `${ms.toFixed(0)} ms`;
  if (ms >= 10) return `${ms.toFixed(1)} ms`;
  return `${ms.toFixed(2)} ms`;
}

function formatRate(perSecond) {
  if (perSecond >= 1e6) {
    const millions = perSecond / 1e6;
    return {
      value: millions >= 100 ? millions.toFixed(0) : millions >= 10 ? millions.toFixed(1) : millions.toFixed(2),
      unit: 'million',
    };
  }
  return { value: fmt.format(Math.round(perSecond / 1000)), unit: 'thousand' };
}

function stressStatus(text) {
  fill(ui.stressOut, h('p', { class: 'stress-status' }, h('span', { class: 'spinner', 'aria-hidden': 'true' }), text));
}

function digestBlock(label, value, expected) {
  const rows = [
    h(
      'div',
      { class: 'digest-row' },
      h('span', { class: 'digest-src' }, 'This browser'),
      h('code', { class: 'digest-hex' }, value),
      h('span'),
    ),
  ];
  if (expected) {
    const same = expected === value;
    rows.push(
      h(
        'div',
        { class: 'digest-row' },
        h('span', { class: 'digest-src' }, 'Native x86-64'),
        h('code', { class: 'digest-hex' }, expected),
        h('span', { class: same ? 'digest-mark' : 'digest-mark is-bad' }, same ? 'same' : 'differs'),
      ),
    );
  }
  return h('div', { class: 'digest' }, h('p', { class: 'digest-label' }, label), rows);
}

function renderStressResult({ count, seed, generationMs, engineMs, summary }) {
  const perSecond = summary.commands / (Math.max(engineMs, 0.001) / 1000);
  const rate = formatRate(perSecond);
  const nsPerCommand = (engineMs * 1e6) / summary.commands;
  const reference = NATIVE.get(`${seed}:${count}`);
  const matches =
    reference && reference.eventDigest === summary.eventDigest && reference.stateHash === summary.stateHash;

  let verdict;
  if (reference && matches) {
    verdict = h(
      'p',
      { class: 'verdict is-match', id: 'stress-verdict', 'data-match': 'true' },
      svgIcon(CHECK),
      'Matches the native x86-64 build',
    );
  } else if (reference) {
    verdict = h(
      'p',
      { class: 'verdict is-mismatch', id: 'stress-verdict', 'data-match': 'false' },
      `Does not match the native reference for seed ${seed} with ${fmt.format(count)} commands.`,
    );
  } else {
    verdict = h(
      'p',
      { class: 'verdict is-none', id: 'stress-verdict' },
      `No native reference is pinned for seed ${seed} with ${fmt.format(count)} commands. Seed 42 with 100,000 or 1,000,000 has one.`,
    );
  }

  fill(
    ui.stressOut,
    h(
      'div',
      { class: 'stress-result' },
      h(
        'div',
        { class: 'rate-block' },
        h(
          'p',
          { class: 'rate' },
          h('span', { class: 'rate-value' }, rate.value),
          h('span', { class: 'rate-unit' }, `${rate.unit} commands per second`),
        ),
        h(
          'p',
          { class: 'rate-note' },
          `Engine throughput, measured in this browser on one thread: about ${nsPerCommand.toFixed(0)} ns per command.`,
        ),
      ),
      h(
        'dl',
        { class: 'metrics' },
        metric('Engine time', formatMs(engineMs)),
        metric('Generation time', formatMs(generationMs)),
        metric('Commands', fmt.format(summary.commands)),
        metric('Events', fmt.format(summary.events)),
        metric('Trades', fmt.format(summary.trades)),
        metric('Resting orders', fmt.format(summary.resting)),
      ),
      h(
        'div',
        { class: 'digests' },
        digestBlock('Event digest', summary.eventDigest, reference?.eventDigest),
        digestBlock('State hash', summary.stateHash, reference?.stateHash),
      ),
      h(
        'div',
        { class: 'verdict-row' },
        verdict,
        h(
          'p',
          { class: 'why' },
          'The digests can match because the browser runs the same Rust code as the native build: integer-only, with no clocks or randomness in the engine.',
        ),
      ),
    ),
  );
  const tail = reference
    ? matches
      ? ' Matches the native x86-64 build.'
      : ' Does not match the native reference.'
    : '';
  announce(`Stress test done: ${rate.value} ${rate.unit} commands per second in this browser.${tail}`);
}

function metric(label, value) {
  return h('div', {}, h('dt', {}, label), h('dd', { class: 'num' }, value));
}

async function runStress() {
  if (state.stressRunning || !state.engine) return;
  const count = state.stressCount;
  const seed = readInt(ui.seed, SEED_MAX);
  ui.seed.value = seed;

  // aria-disabled rather than disabled, so a keyboard user keeps focus on the button.
  state.stressRunning = true;
  ui.stressRun.setAttribute('aria-disabled', 'true');
  ui.stressRun.textContent = 'Running…';
  ui.stressOut.setAttribute('aria-busy', 'true');
  stressStatus(`Generating ${fmt.format(count)} commands with seed ${seed}…`);
  announce(`Stress test started with ${fmt.format(count)} commands.`);
  await nextPaint();

  let playground = null;
  try {
    playground = new Playground();
    const g0 = performance.now();
    playground.stage(count, seed);
    const generationMs = performance.now() - g0;

    stressStatus(`Matching ${fmt.format(count)} commands…`);
    await nextPaint();

    const t0 = performance.now();
    const json = playground.runStaged();
    const engineMs = performance.now() - t0;

    renderStressResult({ count, seed, generationMs, engineMs, summary: JSON.parse(json) });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    fill(
      ui.stressOut,
      h('p', { class: 'stress-error' }, `The stress test stopped before it finished (${message}). Try a smaller run.`),
    );
    announce('The stress test stopped before it finished.');
  } finally {
    try {
      playground?.free();
    } catch {
      // The instance is gone either way.
    }
    ui.stressRun.textContent = 'Run stress test';
    ui.stressOut.setAttribute('aria-busy', 'false');
    // Clicks made while the main thread was busy arrive now; let them hit the guard first.
    await nextPaint();
    state.stressRunning = false;
    ui.stressRun.removeAttribute('aria-disabled');
  }
}

ui.stressCount.addEventListener('click', (event) => {
  const button = event.target.closest('button[data-value]');
  if (!button) return;
  state.stressCount = Number(button.dataset.value);
  for (const option of ui.stressCount.querySelectorAll('button')) {
    option.setAttribute('aria-pressed', option === button ? 'true' : 'false');
  }
});

ui.seed.addEventListener('change', () => {
  ui.seed.value = readInt(ui.seed, SEED_MAX);
});

ui.stressRun.addEventListener('click', runStress);

// ---------------------------------------------------------------------------------------
// Start

async function start() {
  renderTicket();
  renderScenarioList();
  try {
    await init();
    state.engine = new Playground();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    ui.ladder.removeAttribute('aria-busy');
    fill(
      ui.spread,
      h(
        'span',
        { class: 'spread-item' },
        `The engine could not start in this browser (${message}). It needs WebAssembly.`,
      ),
    );
    return;
  }
  ui.ladder.setAttribute('tabindex', '-1');
  ui.ladder.removeAttribute('aria-busy');
  loadScenario(scenarios[0], { quiet: true });
  ui.submit.disabled = false;
  ui.clearBook.disabled = false;
  ui.stressRun.disabled = false;
  document.documentElement.dataset.engine = 'ready';
}

start();
