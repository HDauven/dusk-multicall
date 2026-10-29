import assert from 'node:assert/strict'
import { getEventListeners } from 'node:events'
import { describe, mock, test } from 'node:test'
import {
  aggregate,
  createMulticallFetch,
  decodeAggregate,
  encodeAggregate,
  type Fetch,
  MAX_BATCH,
  MULTICALL_CONTRACT_ID,
  type MulticallFetchOptions,
} from '../src/mod.ts'
import { bytes, countCalls, hex, node, NODE, reply, STAKE, TRANSFER, urls, waitFor } from './helpers.ts'
import recorded from './testnet.json' with { type: 'json' }

const AGGREGATE = `${NODE}/on/contracts:${MULTICALL_CONTRACT_ID}/aggregate`
const ACCOUNT = `${NODE}/on/contracts:${TRANSFER}/account`
// A cap for the stake contract's reads and a fair share for the others, the
// policy testnet's recorded query was sent with.
const gasFor = (id: string) => (id === STAKE ? 50_000_000n : 0n)
// A contract read as clients send it: the argument's bytes, typed as bytes.
const OCTETS = { 'Content-Type': 'application/octet-stream' }
const read = (body = [1, 2, 3], init: RequestInit = {}): [string, RequestInit] => [
  ACCOUNT,
  { method: 'POST', body: new Uint8Array(body), ...init, headers: { ...OCTETS, ...(init.headers as object) } },
]
const batched = (base: Fetch, options: Partial<MulticallFetchOptions> = {}) =>
  createMulticallFetch({ nodeUrl: NODE, multicallId: MULTICALL_CONTRACT_ID, fetch: base, gasFor, ...options })
const body = async (res: Response) => hex(new Uint8Array(await res.arrayBuffer()))
const later = () => new Promise((resolve) => setTimeout(resolve, 10))

test('sends the reads of one task as the query testnet answered, and gives each read its own reply', async () => {
  const base = node((url) => {
    assert.equal(url, AGGREGATE)
    return new Response(bytes(recorded.aggregate.reply), { status: recorded.aggregate.status })
  })
  const fetch = batched(base)
  const answers = await Promise.all(
    recorded.reads.map((r, i) =>
      // A URL works as a string does.
      fetch(i ? `${NODE}${r.url}` : new URL(`${NODE}${r.url}`), { method: 'POST', headers: OCTETS, body: bytes(r.body) })
    ),
  )
  assert.equal(base.mock.callCount(), 1)
  // The envelope is byte for byte the one testnet answered.
  assert.equal(hex(base.mock.calls[0].arguments[1]?.body as Uint8Array), recorded.aggregate.envelope)
  const { results } = decodeAggregate(bytes(recorded.aggregate.reply), recorded.reads.length)
  for (const [i, res] of answers.entries()) {
    assert.equal(res.status, 200)
    assert.equal(await body(res), hex((results[i] as { Ok: Uint8Array }).Ok))
  }
})

test('batches only reads the node takes as bytes', async () => {
  // The node reads any other body as text, `0x` hex or UTF-8, and answers in
  // hex; in a query the bytes would go as they are, and come back as bytes.
  const base = node(() => new Response(bytes('aa')))
  const fetch = batched(base)
  const typed = (n: number, type?: string) =>
    fetch(ACCOUNT, { method: 'POST', body: new Uint8Array([n]), headers: type ? { 'Content-Type': type } : {} })
  await Promise.all([
    typed(1),
    typed(2, 'application/json'),
    typed(3, 'Application/Octet-Stream'),
    typed(4, 'application/octet-stream; charset=binary'),
  ])
  assert.deepEqual(urls(base), [ACCOUNT, ACCOUNT, ACCOUNT, ACCOUNT])
})

test('makes one call of the same read twice', async () => {
  let calls = 0
  const base = node((_, init) => {
    calls = countCalls(init.body)
    return new Response(reply([{ Ok: bytes('0a') }, { Ok: bytes('0b') }]))
  })
  const fetch = batched(base)
  const answers = await Promise.all([fetch(...read([1])), fetch(...read([2])), fetch(...read([1]))])
  assert.equal(calls, 2)
  assert.deepEqual(await Promise.all(answers.map(body)), ['0a', '0b', '0a'])
})

test('takes a contract ID in either case as one contract', async () => {
  const id = 'ab'.repeat(32)
  const base = node((url) => new Response(url === AGGREGATE ? reply([{ Ok: bytes('0a') }, { Ok: bytes('0b') }]) : bytes('aa')))
  const fetch = batched(base, { gasFor: (contract) => (contract === id ? 7n : 0n) })
  const at = (contract: string, n: number) =>
    fetch(`${NODE}/on/contracts:${contract}/account`, { method: 'POST', headers: OCTETS, body: new Uint8Array([n]) })
  const own = `${NODE}/on/contracts:${MULTICALL_CONTRACT_ID.toUpperCase()}/version`
  const answers = await Promise.all([at(id.toUpperCase(), 1), at(id, 1), at(id, 2), fetch(own, { method: 'POST', headers: OCTETS, body: new Uint8Array() })])
  // The same read in either case is one call; the multicall's own functions go as they are.
  assert.deepEqual(urls(base), [own, AGGREGATE])
  assert.deepEqual(await Promise.all(answers.slice(0, 3).map(body)), ['0a', '0a', '0b'])
  const sent = [1, 2].map((n) => ({ contract: bytes(id), fnName: 'account', args: new Uint8Array([n]), gasLimit: 7n }))
  assert.equal(hex(base.mock.calls[1].arguments[1]?.body as Uint8Array), hex(encodeAggregate(sent)))
})

test('sends a read as it was made, whatever its caller changes after', async () => {
  const base = node(() => new Response(bytes('aa')))
  const url = new URL(ACCOUNT)
  const init = { method: 'POST', headers: OCTETS, body: new Uint8Array([1, 2]) }
  const answer = batched(base)(url, init)
  // The read goes in a later task, after its caller reused its URL and buffer.
  url.host = 'elsewhere.test'
  init.body[0] = 9
  await answer
  assert.equal(String(base.mock.calls[0].arguments[0]), ACCOUNT)
  assert.deepEqual(Array.from(base.mock.calls[0].arguments[1]?.body as Uint8Array), [1, 2])
})

test('sends a lone read as it is', async () => {
  const base = node(() => new Response(bytes('aa')))
  assert.equal(await body(await batched(base)(...read())), 'aa')
  assert.deepEqual(urls(base), [ACCOUNT])
})

test('sends a read that ran out of its share of gas again, alone', async () => {
  const base = node((url) =>
    url === AGGREGATE ? new Response(reply([{ Ok: bytes('01') }, { Err: 'OutOfGas' }])) : new Response(bytes('02'))
  )
  const fetch = batched(base)
  const answers = await Promise.all([fetch(...read([1])), fetch(...read([2]))])
  assert.deepEqual(await Promise.all(answers.map(body)), ['01', '02'])
  assert.deepEqual(urls(base), [AGGREGATE, ACCOUNT])
  assert.deepEqual(Array.from(base.mock.calls[1].arguments[1]?.body as Uint8Array), [2])
})

test('answers a read whose call failed with a 500 and the error', async () => {
  const long = 'a panic message longer than seven bytes'
  const base = node(() =>
    new Response(reply([{ Err: { Panic: 'boom' } }, { Err: { Panic: long } }, { Err: 'DoesNotExist' }, { Err: 'Unknown' }]))
  )
  const fetch = batched(base)
  const answers = await Promise.all([1, 2, 3, 4].map((n) => fetch(...read([n]))))
  const got = await Promise.all(answers.map(async (r) => [r.status, await r.text()]))
  assert.deepEqual(got, [[500, 'boom'], [500, long], [500, 'DoesNotExist'], [500, 'Unknown']])
  assert.equal(base.mock.callCount(), 1)
})

test('sends the reads of a query that failed whole alone, holds batching off for 5 minutes after two in a row', async (t) => {
  t.mock.timers.enable({ apis: ['Date'], now: 1_000_000 })
  const base = node((url) => (url === AGGREGATE ? new Response('boom', { status: 500 }) : new Response(bytes('aa'))))
  const fetch = batched(base)
  const pair = () => Promise.all([fetch(...read([1])), fetch(...read([2]))])
  const aggregates = () => urls(base).filter((url) => url === AGGREGATE).length
  for (const res of await pair()) assert.equal(res.status, 200)
  await pair()
  assert.equal(aggregates(), 2)
  await pair()
  // Held off: the third pair went straight to the node.
  assert.equal(aggregates(), 2)
  assert.equal(base.mock.callCount(), 8)
  t.mock.timers.tick(299_999)
  await pair()
  assert.equal(aggregates(), 2)
  t.mock.timers.tick(1)
  await pair()
  assert.equal(aggregates(), 3)
})

test('sends the reads alone after a 503 that says nothing of when to come back', async () => {
  const base = node((url) => (url === AGGREGATE ? new Response('busy', { status: 503 }) : new Response(bytes('aa'))))
  const fetch = batched(base)
  const statuses = (await Promise.all([fetch(...read([1])), fetch(...read([2]))])).map((r) => r.status)
  assert.deepEqual(statuses, [200, 200])
  assert.deepEqual(urls(base), [AGGREGATE, ACCOUNT, ACCOUNT])
})

test('answers every read of a rate-limited query with its status, its wait and the node\'s Date', async () => {
  const cases = [
    [429, '2', null],
    [429, '2', 'Mon, 26 Oct 2026 04:00:00 GMT'],
    [503, '3', null],
    [503, 'Mon, 26 Oct 2026 04:00:05 GMT', 'Mon, 26 Oct 2026 04:00:00 GMT'],
  ] as const
  let current: (typeof cases)[number] = cases[0]
  const base = node(() => {
    const [status, wait, date] = current
    return new Response('slow down', { status, headers: { 'Retry-After': wait, ...(date ? { Date: date } : {}) } })
  })
  // One batcher for every case: a rate limit is the node's answer, not a failed query.
  const fetch = batched(base)
  for (const c of cases) {
    current = c
    const [status, wait, date] = c
    const answers = await Promise.all([fetch(...read([1])), fetch(...read([2]))])
    assert.deepEqual(
      answers.map((r) => [r.status, r.headers.get('Retry-After'), r.headers.get('Date')]),
      [[status, wait, date], [status, wait, date]],
    )
  }
  assert.deepEqual(urls(base), cases.map(() => AGGREGATE))
})

test('lets go of a read\'s signal once the read is answered', async () => {
  const base = node((url) => new Response(url === AGGREGATE ? reply([{ Ok: bytes('01') }, { Ok: bytes('02') }]) : bytes('03')))
  const fetch = batched(base)
  // One signal for a page's whole life.
  const page = new AbortController()
  const answers = await Promise.all([fetch(...read([1], { signal: page.signal })), fetch(...read([2], { signal: page.signal }))])
  answers.push(await fetch(...read([3], { signal: page.signal })))
  assert.deepEqual(await Promise.all(answers.map(body)), ['01', '02', '03'])
  assert.equal(getEventListeners(page.signal, 'abort').length, 0)
})

describe('cancelling', () => {
  // A node that never answers: each request waits until its signal aborts.
  const hanging = () => {
    const signals: AbortSignal[] = []
    const base = mock.fn<Fetch>((_, init) =>
      new Promise((_, reject) => {
        const signal = init?.signal as AbortSignal
        signals.push(signal)
        signal.addEventListener('abort', () => reject(signal.reason))
      })
    )
    return { base, signals }
  }
  const abortable = (n: number) => {
    const controller = new AbortController()
    return { controller, args: read([n], { signal: controller.signal }) }
  }

  test('cancels a query once every read in it gave up, and sends nothing again', async () => {
    const { base, signals } = hanging()
    const fetch = batched(base)
    const reads = [abortable(1), abortable(2)]
    const answers = reads.map(({ args }) => fetch(...args).catch((error) => error.message))
    await waitFor(() => assert.equal(base.mock.callCount(), 1))
    reads[0].controller.abort(new Error('timeout'))
    // One read still waits.
    assert.equal(signals[0].aborted, false)
    reads[1].controller.abort(new Error('timeout'))
    assert.equal(signals[0].aborted, true)
    assert.deepEqual(await Promise.all(answers), ['timeout', 'timeout'])
    // A read that gave up before it was made is never sent.
    await assert.rejects(fetch(...read([3], { signal: AbortSignal.abort(new Error('gone')) })), /gone/)
    await later()
    assert.equal(base.mock.callCount(), 1)
    // A second abandoned query in a row holds batching off, as a failed one does.
    for (const [n, calls] of [[4, 2], [6, 4]]) {
      const pair = [abortable(n), abortable(n + 1)]
      const gaveUp = pair.map(({ args }) => fetch(...args).catch(() => {}))
      await waitFor(() => assert.equal(base.mock.callCount(), calls))
      for (const { controller } of pair) controller.abort()
      await Promise.all(gaveUp)
    }
    assert.deepEqual(urls(base), [AGGREGATE, AGGREGATE, ACCOUNT, ACCOUNT])
  })

  test('keeps a query for a read that can\'t give up', async () => {
    let signal: AbortSignal | undefined
    const base = node((_, init) => {
      signal = init.signal ?? undefined
      return new Response(reply([{ Ok: bytes('01') }, { Ok: bytes('02') }]))
    })
    const fetch = batched(base)
    const { controller, args } = abortable(1)
    const answers = Promise.allSettled([fetch(...args), fetch(...read([2]))])
    controller.abort()
    const [gaveUp, kept] = await answers
    assert.equal(gaveUp.status, 'rejected')
    assert.equal(kept.status === 'fulfilled' && (await body(kept.value)), '02')
    assert.equal(signal?.aborted, false)
  })

  test('keeps a query for reads that share one signal until it aborts', async () => {
    const { base, signals } = hanging()
    const fetch = batched(base)
    const controller = new AbortController()
    const answers = [read([1]), read([2])].map(([url, init]) =>
      fetch(url, { ...init, signal: controller.signal }).catch((error) => error)
    )
    await waitFor(() => assert.equal(base.mock.callCount(), 1))
    assert.equal(signals[0].aborted, false)
    controller.abort()
    await Promise.all(answers)
    assert.equal(signals[0].aborted, true)
  })

  test('cancels a read sent alone for two waiters once both gave up', async () => {
    const { base, signals } = hanging()
    const fetch = batched(base)
    const reads = [abortable(1), abortable(1)]
    const answers = reads.map(({ args }) => fetch(...args).catch((error) => error))
    await waitFor(() => assert.equal(base.mock.callCount(), 1))
    assert.deepEqual(urls(base), [ACCOUNT])
    reads[0].controller.abort()
    assert.equal(signals[0].aborted, false)
    reads[1].controller.abort()
    await Promise.all(answers)
    assert.equal(signals[0].aborted, true)
  })

  test('lets go of a read sent alone once its last reader has read it', async () => {
    let answer = (_: Response) => {}
    const signals: AbortSignal[] = []
    const base = mock.fn<Fetch>((_, init) => {
      signals.push(init?.signal as AbortSignal)
      return new Promise((resolve) => (answer = resolve))
    })
    const fetch = batched(base)
    // Two callers of one read; the first gives up before the node answers.
    const [first, second] = [abortable(1), abortable(1)]
    const gaveUp = fetch(...first.args).catch((error) => error)
    const kept = fetch(...second.args)
    await waitFor(() => assert.equal(base.mock.callCount(), 1))
    first.controller.abort()
    await gaveUp
    answer(new Response(bytes('0a')))
    assert.equal(await body(await kept), '0a')
    assert.equal(getEventListeners(second.controller.signal, 'abort').length, 0)
    second.controller.abort()
    assert.equal(signals[0].aborted, false)
  })

  test('cancels a read sent alone whose body stalls once its reader gives up', async () => {
    const signals: AbortSignal[] = []
    // Headers at once, then a body that only ends when the request aborts.
    const base = mock.fn<Fetch>((_, init) => {
      const signal = init?.signal as AbortSignal
      signals.push(signal)
      const stalled = new ReadableStream({ start: (stream) => signal.addEventListener('abort', () => stream.error(signal.reason)) })
      return Promise.resolve(new Response(stalled))
    })
    const { controller, args } = abortable(1)
    const reading = batched(base)(...args).then((res) => res.arrayBuffer()).catch((error) => error)
    await waitFor(() => assert.equal(base.mock.callCount(), 1))
    await later()
    assert.equal(signals[0].aborted, false)
    controller.abort(new Error('timeout'))
    await waitFor(() => assert.equal(signals[0].aborted, true))
    assert.ok((await reading) instanceof Error)
  })

  test('stops passing a reader\'s abort on once it has read the body', async () => {
    const signals: AbortSignal[] = []
    const base = mock.fn<Fetch>((_, init) => {
      signals.push(init?.signal as AbortSignal)
      return Promise.resolve(new Response(new Uint8Array([7, 8])))
    })
    const fetch = batched(base)
    const reads = [abortable(1), abortable(1)]
    const answers = await Promise.all(reads.map(({ args }) => fetch(...args)))
    assert.deepEqual(await Promise.all(answers.map(body)), ['0708', '0708'])
    for (const { controller } of reads) controller.abort()
    assert.equal(signals[0].aborted, false)
  })
})

test(`puts at most ${MAX_BATCH} reads in a query, each planned at 2 KiB of reply until one was seen`, async () => {
  const sizes: number[] = []
  let empty = false
  const base = node((_, init) => {
    const n = countCalls(init.body)
    sizes.push(n)
    return new Response(reply(Array.from({ length: n }, (_, i) => ({ Ok: new Uint8Array(empty ? [] : [i]) }))))
  })
  const round = async (fetch: Fetch) => {
    sizes.length = 0
    const answers = await Promise.all(Array.from({ length: 70 }, (_, i) => fetch(...read([i, i >> 8]))))
    assert.ok(answers.every((r) => r.status === 200))
    return [...sizes]
  }
  const fetch = batched(base)
  // Before it has seen a reply, a read is planned at 2 KiB, so the reply fits its 64 KiB.
  assert.deepEqual(await round(fetch), [29, 29, 12])
  // With these small replies seen, the cap is the count.
  assert.deepEqual(await round(fetch), [32, 32, 6])
  // Planned at the size the caller gives, the cap binds at once, and maxBatch moves it.
  assert.deepEqual(await round(batched(base, { replyEstimate: () => 1536 })), [32, 32, 6])
  assert.deepEqual(await round(batched(base, { maxBatch: 64, replyEstimate: () => 8 })), [64, 6])
  // A reply of 0 bytes is a size too: once seen, those reads are planned at it.
  empty = true
  const zero = batched(base)
  assert.deepEqual(await round(zero), [29, 29, 12])
  assert.deepEqual(await round(zero), [32, 32, 6])
  // Two 30,000-byte arguments fit one envelope's 64 KiB, a third doesn't: it goes alone.
  const wide = node((url) => new Response(url === AGGREGATE ? reply([{ Ok: bytes('01') }, { Ok: bytes('02') }]) : bytes('03')))
  const large = batched(wide)
  const answers = await Promise.all(
    [1, 2, 3].map((n) => large(ACCOUNT, { method: 'POST', headers: OCTETS, body: new Uint8Array(30_000).fill(n) })),
  )
  assert.deepEqual(await Promise.all(answers.map(body)), ['01', '02', '03'])
  assert.deepEqual(urls(wide), [AGGREGATE, ACCOUNT])
  assert.equal(countCalls(wide.mock.calls[0].arguments[1]?.body), 2)
})

test('sends each read alone when its query can\'t be planned', { timeout: 5000 }, async () => {
  const estimates = [
    () => {
      throw new Error('no estimate')
    },
    () => 1n as unknown as number,
  ]
  for (const replyEstimate of estimates) {
    const base = node(() => new Response(bytes('aa')))
    const fetch = batched(base, { replyEstimate })
    const answers = await Promise.all([fetch(...read([1])), fetch(...read([2]))])
    assert.deepEqual(await Promise.all(answers.map(body)), ['aa', 'aa'])
    assert.deepEqual(urls(base), [ACCOUNT, ACCOUNT])
  }
})

test('leaves every other request alone', async () => {
  const base = node(() => new Response('{}'))
  const fetch = batched(base)
  const own = `${NODE}/on/contracts:${MULTICALL_CONTRACT_ID}/version`
  await Promise.all([
    fetch(`${NODE}/on/graphql/query`, { method: 'POST', body: 'query { x }' }),
    fetch('https://example.com/driver.wasm'),
    // The multicall's own functions, a body that isn't bytes, and a feeder
    // query, whose answer streams.
    fetch(own, { method: 'POST', headers: OCTETS, body: new Uint8Array() }),
    fetch(ACCOUNT, { method: 'POST', headers: OCTETS, body: 'text' }),
    fetch(...read([1], { headers: { 'Rusk-Feeder': 'true' } })),
    fetch(...read([2])),
  ])
  assert.deepEqual(urls(base), [`${NODE}/on/graphql/query`, 'https://example.com/driver.wasm', own, ACCOUNT, ACCOUNT, ACCOUNT])
})

test('batches through the multicall it is given, and not at all without one', async () => {
  const other = 'ab'.repeat(32)
  const reads = async (multicallId: string | null) => {
    const base = node(() => new Response(bytes('aa')))
    const fetch = batched(base, { multicallId })
    await Promise.all([fetch(...read([1])), fetch(...read([2]))])
    return urls(base)
  }
  // None on the node's network, as on a local node: each read goes as it is.
  assert.deepEqual(await reads(null), [ACCOUNT, ACCOUNT])
  // Another network's: one query to it, whose reply here is none, so each read goes alone after it.
  assert.deepEqual(await reads(other.toUpperCase()), [`${NODE}/on/contracts:${other}/aggregate`, ACCOUNT, ACCOUNT])
  for (const multicallId of [undefined, '', 'ab', `0x${other}`]) {
    assert.throws(() => batched(node(() => new Response()), { multicallId: multicallId as string }), TypeError)
  }
})

test('aggregate sends its reads as one query and returns the height and each result', async () => {
  const base = node(() => new Response(reply([{ Ok: bytes('0102') }, { Err: 'OutOfGas' }], 42n)))
  const reads = [
    { contract: TRANSFER, fnName: 'num_notes' },
    { contract: STAKE.toUpperCase(), fnName: 'get_stake', args: bytes('aa'), gasLimit: 5n },
  ]
  assert.deepEqual(await aggregate(reads, { nodeUrl: `${NODE}/`, fetch: base }), {
    height: 42n,
    results: [{ Ok: bytes('0102') }, { Err: 'OutOfGas' }],
  })
  assert.deepEqual(urls(base), [AGGREGATE])
  const sent = encodeAggregate([
    { contract: bytes(TRANSFER), fnName: 'num_notes', args: new Uint8Array(), gasLimit: 0n },
    { contract: bytes(STAKE), fnName: 'get_stake', args: bytes('aa'), gasLimit: 5n },
  ])
  assert.equal(hex(base.mock.calls[0].arguments[1]?.body as Uint8Array), hex(sent))
  // A query that fails whole throws, with what the node said.
  const refused = node(() => new Response('no such function', { status: 500 }))
  await assert.rejects(aggregate(reads, { nodeUrl: NODE, fetch: refused }), /HTTP 500: no such function/)
  await assert.rejects(aggregate(Array(257).fill(reads[0]), { nodeUrl: NODE, fetch: base }), /at most 256 calls/)
  await assert.rejects(aggregate([{ contract: 'aa', fnName: 'f' }], { nodeUrl: NODE, fetch: base }), TypeError)
  assert.equal(base.mock.callCount(), 1)
})
