/**
 * Contract reads through Multicall for Dusk: the reads a client makes in one
 * task go to the node as one `aggregate` query, and each read gets its own
 * reply, read at the same block as the others.
 *
 * The more reads a page makes, the more it saves. On a local Rusk 1.7.1 node,
 * a page of 251 token balances loaded in 26 ms instead of 387 ms, 15 times
 * faster, and the node spent 113 µs of CPU per read instead of 1,328. Small
 * views of the genesis contracts were 4 times faster at 10 reads and 25 times
 * at 256. Queries pay no fee, so batching costs the user nothing.
 *
 * - {@linkcode createMulticallFetch} returns a `fetch` that batches the
 *   contract reads it is given and passes every other request on.
 * - {@linkcode aggregate} sends one list of reads as one query.
 * - `@hdauven/dusk-multicall/codec` encodes the envelope and decodes the
 *   reply, so no data driver is needed for them.
 *
 * @example Three reads of the genesis contracts in one query
 * ```ts
 * import { createMulticallFetch, MULTICALL_CONTRACT_ID } from '@hdauven/dusk-multicall'
 *
 * const nodeUrl = 'https://testnet.nodes.dusk.network'
 * const fetch = createMulticallFetch({ nodeUrl, multicallId: MULTICALL_CONTRACT_ID })
 *
 * // A contract read, as without the multicall: its argument's bytes, POSTed.
 * async function readU64(contract: string, fnName: string, args = new Uint8Array()): Promise<bigint> {
 *   const res = await fetch(`${nodeUrl}/on/contracts:${contract}/${fnName}`, {
 *     method: 'POST',
 *     headers: { 'Content-Type': 'application/octet-stream' },
 *     body: args,
 *   })
 *   if (!res.ok) throw new Error(await res.text())
 *   return new DataView(await res.arrayBuffer()).getBigUint64(0, true)
 * }
 *
 * const transfer = '01'.padEnd(64, '0')
 * const stake = '02'.padEnd(64, '0')
 * const stakeId = new Uint8Array(32)
 * stakeId[0] = 2
 *
 * // Started in one task, so one query carries all three.
 * const [staked, burnt, notes] = await Promise.all([
 *   readU64(transfer, 'contract_balance', stakeId), // the Lux the stake contract holds
 *   readU64(stake, 'burnt_amount'),
 *   readU64(transfer, 'num_notes'),
 * ])
 * ```
 * @module
 */
import {
  ARGBUF_LEN,
  callBytes,
  decodeAggregate,
  encodeAggregate,
  envelopeLength,
  MAX_CALLS,
  MULTICALL_CONTRACT_ID,
  replyLength,
} from './codec.ts'
import type { Aggregate, MulticallResult } from './codec.ts'

export * from './codec.ts'

/**
 * The most reads one query carries unless `maxBatch` says otherwise. Each
 * query also stays within the envelope's 64 KiB and a planned reply of 60,000
 * bytes.
 */
export const MAX_BATCH = 32
/** Reply bytes a query is planned for, below the 65,536 it may take: estimates can be low. */
const REPLY_BUDGET = 60_000
/** A read's reply estimate before one was seen. */
const FIRST_REPLY = 2048
/** Queries that fail whole in a row before batching stops, and for how long. */
const STREAK = 2
const HOLDOFF_MS = 300_000
const OCTETS = { Accept: 'application/octet-stream', 'Content-Type': 'application/octet-stream' }

/** A `fetch`: what the batcher sends its requests with, and what it returns. */
export type Fetch = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>

/** The options of {@linkcode createMulticallFetch}. */
export interface MulticallFetchOptions {
  /** The node: reads posted to `<nodeUrl>/on/contracts:<id>/<fn>` are batched. */
  nodeUrl: string
  /**
   * Multicall for Dusk's contract ID on the node's network, or `null` for a
   * network without it, such as a local node: every request then goes to
   * `fetch` as it is. Mainnet and testnet have it at
   * {@linkcode MULTICALL_CONTRACT_ID}.
   */
  multicallId: string | null
  /** Sends the queries and every other request; the global `fetch` by default. */
  fetch?: Fetch
  /**
   * A read's gas limit inside a query: `0n`, the default, for a fair share of
   * the query's gas. A read that runs out of gas in a query is sent again on
   * its own, so a limit only helps a read known to need more than a share.
   */
  gasFor?: (contractId: string, fnName: string) => bigint
  /** The most reads in one query, at most 256; {@linkcode MAX_BATCH} by default. */
  maxBatch?: number
  /**
   * The reply size in bytes a read is planned for until one of its replies
   * was seen; 2,048 when it returns `undefined`, as by default.
   */
  replyEstimate?: (contractId: string, fnName: string) => number | undefined
}

/** One read for {@linkcode aggregate}. */
export interface Read {
  /** The callee's contract ID, 64 hex characters. */
  contract: string
  /** The function to call. */
  fnName: string
  /** The argument in the callee's own encoding; none by default. */
  args?: Uint8Array
  /** The read's gas limit, `0n` (the default) for a fair share of the query's gas. */
  gasLimit?: bigint
}

/** The options of {@linkcode aggregate}. */
export interface AggregateOptions {
  /** The node to query. */
  nodeUrl: string
  /** Multicall for Dusk's contract ID; {@linkcode MULTICALL_CONTRACT_ID}, mainnet's and testnet's, by default. */
  multicallId?: string
  /** Sends the query; the global `fetch` by default. */
  fetch?: Fetch
  /** Cancels the query. */
  signal?: AbortSignal
}

interface Waiter {
  init: RequestInit
  /** Answered, or its caller gave up. */
  settled: boolean
  resolve: (response: Response) => void
  reject: (reason?: unknown) => void
}
interface Call {
  id: string
  fn: string
  args: Uint8Array
  gas: bigint
  input: RequestInfo | URL
  waiters: Waiter[]
}

/**
 * A `fetch` that sends the contract reads it gets in one task as one
 * Multicall for Dusk query, and answers each read as the node would have
 * answered it alone.
 *
 * A read is a `POST` to `<nodeUrl>/on/contracts:<id>/<fn>` with
 * `Content-Type: application/octet-stream`, whose body is its argument's
 * bytes, as a `Uint8Array`, an `ArrayBuffer` or another buffer view. The reads
 * made before the next task, their microtasks included, go in one query, or in
 * a few when they pass its limits, and the same read made twice is sent once.
 *
 * - A read answered in the query gets status 200 and the callee's reply bytes.
 * - A read whose call failed gets status 500 and the callee's panic message,
 *   or `DoesNotExist` or `Unknown`.
 * - A read that ran out of its share of the query's gas is sent again alone.
 * - When a query fails whole, its reads are sent alone. After two such
 *   queries in a row, reads go alone for 5 minutes.
 * - A rate limit (429, or 503 with a `Retry-After`) answers every read in the
 *   query with its status, `Retry-After` and `Date`.
 * - A query is cancelled once every read in it was aborted; a read without a
 *   signal keeps it.
 * - A read alone in its task, a read of another content type, which the node
 *   reads as text and answers in hex, a read with a `Rusk-Feeder` header and
 *   every other request go to `fetch` as they are.
 *
 * Throws a `TypeError` when `multicallId` is neither a contract ID nor `null`.
 *
 * @example Reads of the transfer contract in one query
 * ```ts
 * import { createMulticallFetch, MULTICALL_CONTRACT_ID } from '@hdauven/dusk-multicall'
 *
 * const nodeUrl = 'https://testnet.nodes.dusk.network'
 * const fetch = createMulticallFetch({
 *   nodeUrl,
 *   multicallId: MULTICALL_CONTRACT_ID, // null on a network without it, such as a local node
 * })
 * const transfer = '01'.padEnd(64, '0')
 * const read = (fnName: string) =>
 *   fetch(`${nodeUrl}/on/contracts:${transfer}/${fnName}`, {
 *     method: 'POST',
 *     headers: { 'Content-Type': 'application/octet-stream' },
 *     body: new Uint8Array(),
 *   })
 *
 * // One query; each response is the one a direct read gets.
 * const [notes, root] = await Promise.all([read('num_notes'), read('root')])
 * ```
 */
export function createMulticallFetch(options: MulticallFetchOptions): Fetch {
  const {
    nodeUrl,
    fetch: base = (input, init) => globalThis.fetch(input, init),
    gasFor = () => 0n,
    maxBatch = MAX_BATCH,
    replyEstimate = () => undefined,
  } = options
  if (options.multicallId === null) return (input, init) => base(input, init)
  const multicall = contractId(options.multicallId, 'multicallId', 'or null')
  const prefix = `${String(nodeUrl).replace(/\/$/, '')}/on/contracts:`
  const limit = Math.min(maxBatch, MAX_CALLS)
  /** The largest reply seen for each contract's function. */
  const sizes = new Map<string, number>()
  let queue: Call[] = []
  let pending = new Map<string, Call>()
  let scheduled = false
  let streak = 0
  let offUntil = -Infinity

  // One more query that failed whole: two in a row turn batching off for a while.
  const strike = () => {
    if (++streak >= STREAK) {
      offUntil = Date.now() + HOLDOFF_MS
      streak = 0
    }
  }

  // A read sent on its own, its answer given to every waiter. Their signals
  // can cancel it until each has read its body, or dropped it.
  const alone = (call: Call) => {
    const watch = whileAnyWaits(call.waiters)
    // The read as it was made: its bytes then, not the caller's buffer now.
    const init = { ...call.waiters[0].init, body: call.args as Uint8Array<ArrayBuffer>, signal: watch.signal }
    new Promise<Response>((resolve) => resolve(base(call.input, init))).then(
      (res) => {
        // A waiter whose caller gave up has its answer: only the others share this one.
        const live = call.waiters.filter((waiter) => !waiter.settled)
        let open = live.length
        const done = () => {
          if (--open === 0) watch.release()
        }
        if (!open) {
          watch.release()
          return discard(res)
        }
        live.forEach((waiter, i) => waiter.resolve(untilRead(i === live.length - 1 ? res : res.clone(), done)))
      },
      (error) => {
        watch.release()
        for (const waiter of call.waiters) waiter.reject(error)
      },
    )
  }

  // A query that failed whole sends its reads alone.
  const failed = (batch: Call[]) => {
    strike()
    for (const call of batch) alone(call)
  }

  const learn = (call: Call, length: number) => {
    const key = `${call.id}/${call.fn}`
    if (length > (sizes.get(key) ?? -1)) sizes.set(key, length)
  }

  // Gives a read of a decoded query its own result.
  const answer = (call: Call, result: MulticallResult) => {
    if ('Ok' in result) {
      learn(call, result.Ok.length)
      for (const waiter of call.waiters) {
        waiter.resolve(new Response(result.Ok.slice(), { headers: { 'Content-Type': OCTETS['Content-Type'] } }))
      }
    } else if (result.Err === 'OutOfGas') {
      // Its share of the query's gas was short: a query of its own has it all.
      alone(call)
    } else {
      const message = typeof result.Err === 'string' ? result.Err : result.Err.Panic
      for (const waiter of call.waiters) waiter.resolve(new Response(message, { status: 500 }))
    }
  }

  // The queued reads in queries that fit the multicall's bounds.
  const plan = (calls: Call[]): Call[][] => {
    const batches: { calls: Call[]; data: number; reply: number }[] = []
    for (const call of calls) {
      const own = callBytes(call.fn, call.args)
      const planned = sizes.get(`${call.id}/${call.fn}`) ?? replyEstimate(call.id, call.fn) ?? FIRST_REPLY
      const last = batches.at(-1)
      if (
        last &&
        last.calls.length < limit &&
        envelopeLength(last.calls.length + 1, last.data + own) <= ARGBUF_LEN &&
        replyLength(last.calls.length + 1, last.reply + planned) <= REPLY_BUDGET
      ) {
        last.calls.push(call)
        last.data += own
        last.reply += planned
      } else {
        batches.push({ calls: [call], data: own, reply: planned })
      }
    }
    return batches.map((batch) => batch.calls)
  }

  const send = async (batch: Call[]) => {
    // A batch of one gains nothing from the multicall.
    if (batch.length === 1) return alone(batch[0])
    let envelope: Uint8Array
    try {
      envelope = encodeAggregate(
        batch.map((call) => ({ contract: fromHex(call.id), fnName: call.fn, args: call.args, gasLimit: call.gas })),
      )
    } catch {
      return failed(batch)
    }
    // Cancelled once every read in it gave up, so abandoned queries don't pile up.
    const watch = whileAnyWaits(batch.flatMap((call) => call.waiters))
    let results: MulticallResult[]
    try {
      const res = await base(`${prefix}${multicall}/aggregate`, {
        method: 'POST',
        headers: OCTETS,
        body: envelope as Uint8Array<ArrayBuffer>,
        signal: watch.signal,
      })
      if (isRateLimit(res)) {
        discard(res)
        return rateLimited(batch, res)
      }
      if (!res.ok) {
        discard(res)
        throw new Error(`HTTP ${res.status}`)
      }
      results = decodeAggregate(new Uint8Array(await res.arrayBuffer()), batch.length).results
    } catch {
      // Nobody waits for the reads of a query they all gave up on: they aren't sent again.
      return watch.signal.aborted ? strike() : failed(batch)
    } finally {
      watch.release()
    }
    streak = 0
    batch.forEach((call, i) => answer(call, results[i]))
  }

  const flush = () => {
    const calls = queue
    scheduled = false
    queue = []
    pending = new Map()
    let batches: Call[][]
    try {
      batches = plan(calls)
    } catch {
      // A replyEstimate that throws, or answers what the plan can't add up: each read goes alone.
      batches = calls.map((call) => [call])
    }
    for (const batch of batches) void send(batch)
  }

  // The read a request is, or null for one that goes to `fetch` as it is.
  const readOf = (input: RequestInfo | URL, init: RequestInit) => {
    if (Date.now() < offUntil || String(init.method).toUpperCase() !== 'POST') return null
    const url = String((input as { href?: unknown }).href ?? (input as { url?: unknown }).url ?? input)
    const match = url.startsWith(prefix) ? /^([0-9a-f]{64})\/([A-Za-z0-9_]+)$/i.exec(url.slice(prefix.length)) : null
    const id = match?.[1].toLowerCase()
    const args = match && id !== multicall ? bodyBytes(init.body) : null
    if (!match || !id || !args || !takesBytes(init.headers)) return null
    // A URL object can change after the call; its address then can't.
    return { id, fn: match[2], args, input: (input as { href?: unknown }).href === undefined ? input : url }
  }

  return (input, init) => {
    const read = readOf(input, init ?? {})
    if (!read) return base(input, init)
    return new Promise<Response>((resolve, reject) => {
      const signal = init?.signal
      if (signal?.aborted) return reject(signal.reason)
      const waiter: Waiter = { init: { ...init }, settled: false, resolve, reject }
      const onAbort = () => waiter.reject(signal?.reason)
      // A waiter settles once, with its answer or when its caller gives up.
      const settling = <T>(settle: (value: T) => void) => (value: T) => {
        if (waiter.settled) return
        waiter.settled = true
        signal?.removeEventListener('abort', onAbort)
        settle(value)
      }
      waiter.resolve = settling(resolve)
      waiter.reject = settling(reject)
      signal?.addEventListener('abort', onAbort, { once: true })
      const key = `${read.id}/${read.fn}/${toHex(read.args)}`
      let call = pending.get(key)
      // The same read twice in one task is one call.
      if (!call) {
        call = { ...read, gas: gasFor(read.id, read.fn), waiters: [] }
        pending.set(key, call)
        queue.push(call)
      }
      call.waiters.push(waiter)
      if (!scheduled) {
        scheduled = true
        nextTask(flush)
      }
    })
  }
}

/**
 * Sends `reads` as one `aggregate` query, as they are: no batching, splitting
 * or retries. Returns the height of the block whose state the query read and
 * each read's result, in order.
 *
 * Throws for more than 256 reads or an envelope over 64 KiB, when the node
 * answers with an HTTP error (its status and text are in the message), and
 * for a reply that isn't one.
 *
 * @example The transfer contract's note count and root at one block
 * ```ts
 * import { aggregate } from '@hdauven/dusk-multicall'
 *
 * const transfer = '01'.padEnd(64, '0')
 * const { height, results } = await aggregate(
 *   [{ contract: transfer, fnName: 'num_notes' }, { contract: transfer, fnName: 'root' }],
 *   { nodeUrl: 'https://testnet.nodes.dusk.network' },
 * )
 * for (const result of results) {
 *   if ('Ok' in result) console.log(height, result.Ok)
 *   else console.log(height, result.Err)
 * }
 * ```
 */
export async function aggregate(reads: readonly Read[], options: AggregateOptions): Promise<Aggregate> {
  const { nodeUrl, fetch: send = (input, init) => globalThis.fetch(input, init), signal } = options
  const multicall = contractId(options.multicallId ?? MULTICALL_CONTRACT_ID, 'multicallId')
  const envelope = encodeAggregate(
    reads.map((read) => ({
      contract: fromHex(contractId(read.contract, 'A read\'s contract')),
      fnName: read.fnName,
      args: read.args ?? new Uint8Array(),
      gasLimit: read.gasLimit ?? 0n,
    })),
  )
  const res = await send(`${String(nodeUrl).replace(/\/$/, '')}/on/contracts:${multicall}/aggregate`, {
    method: 'POST',
    headers: OCTETS,
    body: envelope as Uint8Array<ArrayBuffer>,
    signal,
  })
  if (!res.ok) throw new Error(`The multicall query failed with HTTP ${res.status}: ${await res.text()}`)
  return decodeAggregate(new Uint8Array(await res.arrayBuffer()), reads.length)
}

/** A contract ID in lowercase, or a TypeError naming what it is for. */
function contractId(id: unknown, what: string, or = ''): string {
  if (typeof id === 'string' && /^[0-9a-f]{64}$/i.test(id)) return id.toLowerCase()
  throw new TypeError(`${what} must be a contract ID, 64 hex characters${or ? `, ${or}` : ''}`)
}

const fromHex = (hex: string) => Uint8Array.from(hex.match(/../g) ?? [], (b) => Number.parseInt(b, 16))
const toHex = (bytes: Uint8Array) => Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')

/** A request body's bytes, copied, or null for a body that isn't a buffer. */
function bodyBytes(body: RequestInit['body']): Uint8Array | null {
  if (body instanceof ArrayBuffer) return new Uint8Array(body.slice(0))
  if (ArrayBuffer.isView(body)) {
    return new Uint8Array(body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength))
  }
  return null
}

/**
 * Whether the node takes a read with these headers as bytes and answers it in
 * bytes, as a query answers: only for exactly `application/octet-stream`, the
 * node's own test. A read of any other type, which the node reads as text,
 * and a feeder query, whose answer streams, go as they are.
 */
function takesBytes(headers: RequestInit['headers']): boolean {
  try {
    const all = new Headers(headers)
    return all.get('Content-Type') === OCTETS['Content-Type'] && !all.has('Rusk-Feeder')
  } catch {
    // Headers `fetch` refuses: the request goes to it as it is, and fails there.
    return false
  }
}

/**
 * Runs `task` at the next task, after every microtask of this one, without
 * the throttling a background tab puts on timers: through a MessageChannel,
 * whose port is closed once used so a runtime can still exit, else a timer.
 */
function nextTask(task: () => void): void {
  if (typeof MessageChannel === 'function') {
    const channel = new MessageChannel()
    channel.port1.onmessage = () => {
      channel.port1.close()
      task()
    }
    channel.port2.postMessage(null)
    return
  }
  setTimeout(task, 0)
}

/**
 * Whether a reply is the node limiting its rate: a 429, or a 503 that says
 * when to come back, in seconds or as an HTTP date. Any other 503 is a failed
 * query.
 */
function isRateLimit(res: Response): boolean {
  if (res.status !== 503) return res.status === 429
  const wait = res.headers.get('Retry-After')?.trim() ?? ''
  return /^\d+$/.test(wait) || (Number.isFinite(Date.parse(wait)) && /[a-z]/i.test(wait))
}

/**
 * A rate limit is the node's answer to every read in the query: each gets its
 * wait, and the node's `Date`, which a wait given as a date counts from.
 */
function rateLimited(batch: Call[], res: Response): void {
  const headers: Record<string, string> = { 'Retry-After': res.headers.get('Retry-After') ?? '' }
  const date = res.headers.get('Date')
  if (date) headers.Date = date
  for (const waiter of batch.flatMap((call) => call.waiters)) {
    waiter.resolve(new Response('Rate limited', { headers, status: res.status }))
  }
}

/** Lets the connection go without reading a body nobody needs. */
function discard(res: Response): void {
  res.body?.cancel().catch(() => {})
}

/**
 * A signal for a request several waiters share: it aborts once each of their
 * own signals has, as nobody waits for the answer then. A waiter without a
 * signal keeps the request, and a signal two waiters share counts once.
 * `release` stops listening to theirs.
 */
function whileAnyWaits(waiters: Waiter[]): { signal: AbortSignal; release: () => void } {
  const controller = new AbortController()
  const signals = waiters.map(({ init }) => init.signal)
  if (signals.some((signal) => !signal)) return { signal: controller.signal, release: () => {} }
  const waiting = new Set(signals as AbortSignal[])
  const watched = [...waiting]
  const onAbort = () => {
    for (const signal of waiting) if (signal.aborted) waiting.delete(signal)
    if (!waiting.size) controller.abort()
  }
  for (const signal of watched) signal.addEventListener('abort', onAbort)
  onAbort()
  return {
    signal: controller.signal,
    release: () => {
      for (const signal of watched) signal.removeEventListener('abort', onAbort)
    },
  }
}

/**
 * The response with a body that calls `done` once, when it has been read to
 * its end, failed or been cancelled; at once when it has none.
 */
function untilRead(res: Response, done: () => void): Response {
  if (!res.body) {
    done()
    return res
  }
  const reader = res.body.getReader()
  let open = true
  const close = () => {
    if (open) {
      open = false
      done()
    }
  }
  return new Response(
    new ReadableStream<Uint8Array>({
      cancel: (reason) => {
        close()
        return reader.cancel(reason)
      },
      pull: async (stream) => {
        try {
          const chunk = await reader.read()
          if (chunk.done) {
            close()
            stream.close()
          } else {
            stream.enqueue(chunk.value)
          }
        } catch (error) {
          close()
          stream.error(error)
        }
      },
    }),
    { headers: res.headers, status: res.status, statusText: res.statusText },
  )
}
