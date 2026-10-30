# @hdauven/dusk-multicall

A JavaScript client for [Multicall for Dusk](https://github.com/HDauven/dusk-multicall).
The contract reads a client makes in one task go to the node as one `aggregate`
query, and each read gets its own reply, read at the same block as the others.

The more reads a page makes, the more it saves. On a local Rusk 1.7.1 node, a
page of 251 token balances loaded in 26 ms instead of 387 ms, 15 times faster,
and the node spent 113 µs of CPU per read instead of 1,328. Small views of the
genesis contracts were 4 times faster at 10 reads and 25 times at 256. Queries
pay no fee, so batching costs the user nothing.

The package has no dependencies and uses only web APIs. It runs in browsers,
extension service workers, Node, Deno, Bun and Cloudflare Workers. In a
Worker, create the fetch inside the request handler: a Worker can't hand one
request's I/O to another.

## Install

From npm:

```sh
npm install @heindauven/dusk-multicall
```

From JSR:

```sh
deno add jsr:@hdauven/dusk-multicall
npx jsr add @hdauven/dusk-multicall
```

It's the same code under two names: `@heindauven/dusk-multicall` on npm, built to JavaScript with its type declarations, and `@hdauven/dusk-multicall` on JSR. The examples below use the JSR name; with npm, import `@heindauven/dusk-multicall` and `@heindauven/dusk-multicall/codec`.

## Example

`createMulticallFetch` returns a `fetch`. Contract reads sent through it in one
task share one query, and every other request goes on as it is. This reads two
balances and a count from the genesis transfer and stake contracts on testnet:

```js
import { createMulticallFetch, MULTICALL_CONTRACT_ID } from '@hdauven/dusk-multicall'

const nodeUrl = 'https://testnet.nodes.dusk.network'
const fetch = createMulticallFetch({ nodeUrl, multicallId: MULTICALL_CONTRACT_ID })

// A contract read, as without the multicall: its argument's bytes, POSTed.
async function readU64(contract, fnName, args = new Uint8Array()) {
  const res = await fetch(`${nodeUrl}/on/contracts:${contract}/${fnName}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/octet-stream' },
    body: args,
  })
  if (!res.ok) throw new Error(await res.text())
  return new DataView(await res.arrayBuffer()).getBigUint64(0, true)
}

const transfer = '01'.padEnd(64, '0')
const stake = '02'.padEnd(64, '0')
const stakeId = new Uint8Array(32)
stakeId[0] = 2

// Started in one task, so one query carries all three.
const [staked, burnt, notes] = await Promise.all([
  readU64(transfer, 'contract_balance', stakeId), // the Lux the stake contract holds
  readU64(stake, 'burnt_amount'),
  readU64(transfer, 'num_notes'),
])
console.log({ staked, burnt, notes })
```

A read's argument and reply are in the callee's own encoding, as in a direct
query. Encode and decode them with that contract's data driver, shipped with
your app. Only reads sent as `application/octet-stream` are batched, since the
node reads any other body as text and answers it in hex.

## Options

| Option | Default | |
| --- | --- | --- |
| `nodeUrl` | required | The node. Reads posted to `<nodeUrl>/on/contracts:<id>/<fn>` are batched. |
| `multicallId` | required | Multicall for Dusk's contract ID on the node's network: `MULTICALL_CONTRACT_ID` on mainnet and testnet, `null` on a network without it, such as a local node. With `null`, every request goes to `fetch` as it is. |
| `fetch` | the global `fetch` | Sends the queries and every other request. |
| `gasFor(contractId, fnName)` | `0n` | A read's gas limit inside a query. `0n` gives it a fair share of the query's gas. |
| `maxBatch` | `32` | The most reads in one query, at most 256. |
| `replyEstimate(contractId, fnName)` | 2,048 | The reply size in bytes a read is planned for until one of its replies was seen. |

## What a read gets

- An answer from the query: status 200 and the callee's reply bytes.
- A call that failed in the query: status 500, with the panic message,
  `DoesNotExist` or `Unknown` as the body.
- A read that ran out of its share of gas is sent again alone.
- When a query fails whole, its reads are sent alone. After two such queries
  in a row, reads go alone for 5 minutes.
- A rate limit (429, or 503 with `Retry-After`) answers every read in the
  query with its status, `Retry-After` and `Date`.
- A query is cancelled once every read in it was aborted. A read without a
  signal keeps it.
- The same read made twice in one task is sent once.
- A read alone in its task, a read of another content type, a read with a
  `Rusk-Feeder` header, a read of the multicall itself and every other request
  go to `fetch` as they are.

Each query stays within the multicall's limits: 256 calls, 64 KiB of envelope,
and a reply planned at 60,000 bytes from the largest reply seen of each
function. Inside a query, a callee's caller is the multicall, so a view that
answers its caller differently answers differently there.

## One query: `aggregate`

`aggregate` sends a list of reads as one query, with no batching, splitting or
retries. It throws when the query fails whole.

```js
import { aggregate } from '@hdauven/dusk-multicall'

const transfer = '01'.padEnd(64, '0')
const { height, results } = await aggregate(
  [{ contract: transfer, fnName: 'num_notes' }, { contract: transfer, fnName: 'root' }],
  { nodeUrl: 'https://testnet.nodes.dusk.network' },
)
// Each result is { Ok: Uint8Array } or { Err: 'OutOfGas' | 'DoesNotExist' | 'Unknown' | { Panic: string } }.
```

## The codec

`@hdauven/dusk-multicall/codec` encodes the `aggregate` envelope and decodes its
reply by hand, as the rkyv archives the contract reads and writes, so no data
driver is needed for them. The tests check it against the contract's own types
and its data driver.

## Development

```sh
node --test test/*.test.ts    # Node 24 or later
deno publish --dry-run
node smoke.ts                 # two read-only queries to testnet
```

The driver test needs `../target/wasm/dusk_multicall_driver.wasm`, from
`make wasm` or the release, and is skipped without it. A `js-v*` tag publishes
the version in `jsr.json` to JSR, with provenance.
