# Dusk Multicall

One contract query that makes many contract reads, all at one state. `aggregate`
takes a list of calls and returns the block height with each call's raw reply
or error, in call order. A failing call never stops the others. The contract
holds no state, its owner has no on-chain powers over it, it can't be upgraded,
and it refuses to run unless a query calls it directly.

The number that decides a page's cost is **node queries per user action**. By
default a Rusk 1.7.1 node answers 180 contract queries per second (bursts of
360, 12 at a time), shared by every client of that node. A page that reads 40
values sends 40 queries and takes 40 of that budget. Through the multicall it
sends one, and the 40 values come back from one block. One query gives one
state: every value in the reply is read at the same block height, so they agree
with each other. Queries pay no fee; gas is only a per-query bound, so batching
costs the user nothing.

## When to use it

Use it for independent reads a client would otherwise send one by one:

- a client library that batches every contract read made in one tick into one
  query, as viem's `batch.multicall` does;
- a page load;
- reads that must agree with each other, such as the Dusk Domains SDK's quote,
  policy config and store admission before it signs.

A purpose-built aggregator, like the Dusk Domains read lens, is the better tool
when a read depends on another read's result (the multicall can't feed one
reply into the next call's argument), or when the answer should be compact
and already checked. The multicall returns every raw reply. The lens's
`name_page` makes up to 271 reads, many of them chosen by earlier replies,
refuses a state that contradicts itself and returns only what the page shows.

## Using it from a client

A client encodes each inner call's argument with **that callee's** data driver,
wraps the calls in one envelope with the **multicall's** driver, sends one
query, then decodes each inner reply with the callee's driver again. That
works for every contract that publishes a compatible data driver, a
`data-driver-js` Wasm for its JSON ↔ rkyv encoding, as the genesis transfer and
stake contracts and the Dusk Domains contracts do. For a contract without one,
encode the argument by hand as the rkyv 0.7 archive (`size_32`) of the
function's argument type, for example with the contract's own Rust types, and
decode the reply bytes the same way. This is the same shape as Ethereum's
Multicall3, where each inner call carries ABI-encoded calldata and viem's
`multicall` encodes and decodes it for you.

This batches two real Dusk Domains reads (the directory's config and the read
lens's `name_page`) and one transfer-contract read, decoding each with its own
driver:

```js
// driver(wasm) returns { encode(fn, json) -> Uint8Array, decode(fn, bytes) -> any }.
// Any Dusk data-driver-js Wasm works; w3sper's data-driver loader does this.
const multicall = driver(await load('dusk_multicall_driver.wasm'))
const directory  = driver(await load('dusk-domains-directory.data-driver.wasm'))
const lens       = driver(await load('dusk-domains-lens.data-driver.wasm'))
const transfer   = driver(await load('transfer-contract.data-driver.wasm'))

// 1. Encode each call's argument with the callee's own driver.
const calls = [
  { contract: DIRECTORY, fn_name: 'config',    fn_args: hex(directory.encode('config', null)),                            gas_limit: '0' },
  { contract: LENS,      fn_name: 'name_page',  fn_args: hex(lens.encode('name_page', { name: 'frozen-proof.dusk', hint: null })), gas_limit: '0' },
  { contract: TRANSFER,  fn_name: 'account',    fn_args: hex(transfer.encode('account', PUBLIC_KEY)),                      gas_limit: '0' },
]

// 2. Encode the envelope with the multicall's driver and send one query.
const envelope = multicall.encode('aggregate', calls)
const reply = await post(`/on/contracts:${MULTICALL}/aggregate`, envelope) // octet-stream

// 3. Decode the envelope, then each inner reply with the callee's driver.
//    Test the discriminant, not the payload: an empty `Ok` is a valid reply
//    (a function that returns `()`) and still has to be decoded.
const { height, results } = multicall.decode('aggregate', reply)
const settle = (result, callee, fn) =>
  Object.hasOwn(result, 'Ok') ? callee.decode(fn, bytes(result.Ok)) : { error: result.Err }
const config   = settle(results[0], directory, 'config')
const namePage = settle(results[1], lens, 'name_page')
const account  = settle(results[2], transfer, 'account')
// All three were read at block `height`.
```

### How decoding works

Each `results[i].Ok` holds exactly the bytes the callee returned to the
multicall; the multicall neither wraps nor re-encodes them. For a view that
doesn't branch on its caller, they equal what a direct query of that call
returns, so the driver that decodes a direct reply decodes the batched one. A
function that answers its caller differently (in a batch, its caller is the
multicall, not `None`) may return other bytes, but they are still that
function's output, and the same driver decodes them.

Encoding is the mirror image: the inner argument must already be in the
callee's encoding before it goes into the envelope, so the client encodes it
with the callee's driver, just as a Multicall3 caller ABI-encodes each inner
call's calldata.

A node can also decode JSON itself, but only for the one contract the request
names, using the driver that node holds for it. Send `aggregate` as JSON and
the node decodes the **envelope** with the multicall's driver; the inner
replies inside it stay hex, because the node runs only the multicall's driver,
not each callee's. So JSON mode through the node is a convenience for the
envelope; the inner values are still decoded client-side with each callee's
driver. (This matches Multicall3: an `eth_call` to it returns each inner result
as raw bytes, and the caller decodes them per call.) JSON mode also trusts that
node's driver; a client that needs integrity decodes locally, in binary mode,
with a pinned driver (see Trust model).

## ABI

| Export | Argument | Reply |
| --- | --- | --- |
| `aggregate` | `Vec<Call>` | `Aggregate` |
| `version` | ignored | `1u32` |

Both run only as direct queries. The contract is written with Dusk Forge
(`#[dusk_forge::contract]`), which generates the Wasm export wrappers and the
`data-driver-js` build that is this envelope's data driver.

```rust
pub struct Call {
    pub contract: ContractId,
    pub fn_name: String,
    pub fn_args: Vec<u8>, // the callee's own encoding
    pub gas_limit: u64,   // 0 = a fair share, see Gas
}

pub struct Aggregate {
    pub height: u64,
    pub results: Vec<Result<Vec<u8>, CallError>>, // one per call, in order
}

pub enum CallError { Panic(String), OutOfGas, DoesNotExist, Unknown }
```

Arguments and replies are rkyv 0.7 archives with `size_32`, the encoding
dusk-core uses. In the driver's JSON, contract IDs and bytes are hex and `u64`
values are decimal strings (numbers are accepted on input):

```json
[{ "contract": "0100000000000000000000000000000000000000000000000000000000000000",
   "fn_name": "account", "fn_args": "<hex>", "gas_limit": "0" }]

{ "height": "28", "results": [{ "Ok": "<hex>" }, { "Err": { "Panic": "boom" } }, { "Err": "OutOfGas" }] }
```

What `aggregate` does, in order:

1. Refuses with `multicall: direct queries only` if any contract called it
   (`abi::caller()` is not `None`). On Rusk 1.7.1 every transaction reaches its
   contract through the transfer contract's `spend_and_execute`, so this
   refuses every transaction, every contract caller and every nested multicall.
   Only a query calls a contract with no caller.
2. Takes the calls as an owned `Vec<Call>`: Forge decodes the argument before
   the method runs.
3. Calls each contract in order with `call_raw_with_limit`, at the call's cap
   or, for a cap of 0, at a fair share of the gas left (see Gas). A reply is
   exactly the bytes the callee returned, which for a view that doesn't branch
   on its caller are the bytes a direct query returns.
4. Puts every call's outcome in `results`, its reply or its error. A failing
   call never stops the batch, and the VM reverts whatever it wrote, so later
   calls never see it. A `Panic` message is kept to at most 200 bytes, cut at a
   character boundary, so a batch of failures always fits in the reply.
5. Returns `height`, the height of the block whose state the query read. Rusk
   1.7.1 opens a query at the tip, reading its height and state root together.

**Why it refuses transactions.** A callee sees the multicall as its caller,
never the user, so a batched call can't carry a user's authority anyway.
Refusing every caller but a query keeps the multicall out of every place where
state is kept, so it can't act on anyone's authority or move anyone's funds.
This holds for both transaction kinds by the one code path: `execute` runs
every Moonlight and Phoenix transaction through the transfer contract's
`spend_and_execute`, which is the caller the multicall sees and refuses. The
E2E run exercises it with Moonlight transactions (the node runs with Phoenix
disabled, as mainnet does after Boreas); the Phoenix case rests on that shared
path, not on a separate test.

A whole query fails in these cases:

| Failure | When |
| --- | --- |
| `multicall: direct queries only` | a transaction or a contract called it |
| `Argument should correctly deserialize: …` | the argument is not a valid `Vec<Call>` archive (the ABI layer rejects it) |
| `multicall: {n} calls, at most 256` | more than 256 calls |
| `multicall: reply exceeds 65536 bytes at call {i}` | the replies and panic messages through call `i` pass 64 KiB |
| VM `OutOfGas` | the query itself used up its gas bound |
| VM `Memory access out of bounds` | an argument over 64 KiB, refused before the contract runs |

Rusk 1.7.1 answers each of them with HTTP 500 and
`{"error":"Internal server error"}`. The message only reaches the node's log,
so a client can't tell these failures from a node fault. Every other failure
belongs to one call and comes back in `results`.

A batch is only as available as the contracts in it: a hostile callee can still
make the whole query fail. Keep calls to contracts you don't trust in a separate
batch.

A callee in a batch differs from a directly queried one in three ways:

- `abi::caller()` is the multicall, not `None`. A view that branches on its
  caller answers differently. The multicall itself is one, and refuses.
- `abi::limit()` is the call's cap or fair share. A cap at or above the gas
  left gets 93% of it instead.
- The calls run one after another in one session. A call that writes changes
  what the later calls in the batch read, and nothing persists after the
  query. Views don't write, so for views this never shows.

## Limits

- 256 calls per batch.
- 64 KiB in and 64 KiB out, the size of the VM's argument buffer. The largest
  argument a single call can carry is 65,472 bytes, the largest single reply
  65,504.
- 3,000,000,000 gas per query, the block gas limit Rusk 1.7.1 gives every
  query.
- A callee `Panic` message is kept to its first 200 bytes.
- No streaming functions. A function that streams its answer with `feed`,
  like the stake contract's `stakes`, fails inside a batch with `Unknown`,
  because an ordinary query has no stream to write to. A client must not send
  `aggregate` as a feeder request (`Rusk-Feeder`); a feeder query passes the
  direct-query guard but has no `results`, and a callee's `feed` chunks would
  stream straight to the client, outside the envelope and its accounting.

The call cap isn't needed for safety, since gas bounds the work. It holds a
batch to about one tick's reads, gives a clear refusal well before the input
buffer's own bound (1,170 calls with no argument and a name of at most 8
bytes), and sits where the buffer binds anyway for views that take a public
key.

How many typical reads fit one query (measured on Rusk 1.7.1):

| Read | Envelope bytes per call | Result bytes per call | Fit by input | Fit by output | Fit by gas | Per query |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| transfer `account(pk)` | 256 | 16 | 255 | 2,047 | 313,807 | 255 |
| stake `get_stake(pk)` | 265 | 56 | 247 | 910 | 93,990 | 247 |

A call that passes a public key takes 256 to 265 bytes of envelope, so the
input buffer binds just before the 256-call cap. Gas never binds for views.

## Gas

Queries pay no fee. Gas is only a bound, and Rusk 1.7.1 stops a query at
3,000,000,000 gas. Nothing below costs a user anything; it describes how a
batch shares that bound.

A cap below the gas left is the callee's limit, passed to the VM unchanged
(piecrust 0.31, the VM in Rusk 1.7.1). A cap at or above the gas left gets the
VM's default instead, 93% of the gas left.

A cap of 0 gets a fair share, the gas left when the call starts divided by
the calls left, itself included, plus one. The extra share stays back for
encoding the reply, so however the calls end, the multicall keeps at least
one share for it. A lone call therefore gets half the bound, 1,500,000,000
gas, and a call that needs more needs an explicit cap. A share is never 0,
because the VM would read 0 as its default; a share that rounds down to 0 is
1, and that call runs out of gas.

A failed call is charged its whole limit, whatever it spent, and a call that
succeeds is charged what it spent. Under fair shares a failure takes only its
own share, so every later call still gets about the query's gas divided by the
batch size plus one. On the node, a probe call got this much gas:

| Probe | Gas | 3,000,000,000 ÷ (calls + 1) |
| --- | ---: | ---: |
| a lone call | 1,499,997,444 | 1,500,000,000 |
| 1st to 4th of 4 cheap calls | 599,998,001, 749,996,196, 999,993,894, 1,499,989,408 | 600,000,000 for the first |
| 6th of 7, after 5 failures | 374,997,235 | 375,000,000 |
| 51st of 52, after 50 failures | 56,599,681 | 56,603,773 |
| 255th of 256, after 254 failures | 11,667,839 | 11,673,151 |

The shares of cheap calls grow, because each leaves most of its share unused.
After 5, 50 and 255 failures a cheap call still succeeds.

A call that needs more than its share needs an explicit cap. In a 256-call
batch a share is about 11.7 million gas, and a lone call's is 1.5 billion.
The probe's `spin(1,000,000)` needs 50 million, so in a full batch it runs out
of gas at cap 0 and succeeds with a cap of 200 million, and the other 255
calls succeed either way.

The share kept back covers the largest reply. The worst case is a full batch
whose calls fail at their shares, the last one included. On the node, 255
failures left 23,334,720 gas, the last call's share was 11,667,446, and a
failing last call leaves 11,667,274 for the encoding, while a full 64 KiB of
replies takes about 4.6 million. A 61,424-byte reply, the largest the buffer
holds next to 255 failed results, came back with the 255 failures before or
after it; one byte more and the buffer refuses the reply, not the gas.

Explicit caps at or above the gas left still take 93% each when their call
fails, so five such failures use up the query. A call to a contract that
doesn't exist takes only the loop's own gas, because the VM refuses it before
it hands over any.

How much of the bound a batch uses. The first rows come from the in-VM
tests, the rest from the node:

| | Gas |
| --- | ---: |
| `aggregate([])` | 3,598 |
| `aggregate([echo; 256])` | 1,175,804 (4,575 per call) |
| each reply byte | about 70 |
| setup before the first call | 6,947 |
| transfer `account(pk)`, measured inside a batch | 9,560 |
| stake `get_stake(pk)`, measured inside a batch | 31,918 |
| transfer `root()`, measured inside a batch | 1,385 |

256 small calls use 0.04% of the bound. Reply bytes take the most, because
rkyv copies a `Vec<u8>` one element at a time, and a full 64 KiB of replies
uses about 4.6 million gas, 0.15% of the bound.

## Trust model

- **Pin the code.** A contract ID is BLAKE2b-256(Wasm ‖ nonce as 8 bytes,
  little-endian ‖ owner bytes), and no transaction can change deployed code. A
  client that pins the ID with the reviewed Wasm's SHA-256, the nonce and the
  owner pins the code, and anyone can recompute the ID from the published Wasm.
- **Results are as good as direct reads.** Each one is the callee's own answer,
  read in one session at one state root. The node is the trust root for both;
  Dusk nodes return no state proofs.
- **A height is not a block.** The reply carries the height, not a block hash.
  A query runs at the node's tip, which may not be final yet, and if that block
  is replaced by another at the same height, nothing in the reply shows it.
  Only a node that reports the block it answered from can close that gap; Rusk
  1.7.1 doesn't.
- **It can only read.** The Wasm imports six host functions (`caller`, `c`,
  `hd`, `limit`, `spent` and `panic`), none that emit events, feed data or
  move value. It has no `init` and no state. The owner has no on-chain powers:
  no transaction can change the code, and there is nothing to administer.
- **JSON mode trusts the node's driver.** A node encodes and decodes JSON with
  whatever driver it holds for the contract, and the owner can replace that
  driver on any node without changing the contract or its ID, so pinning the
  contract does not pin the driver. A client that needs integrity sends binary
  queries and runs a pinned copy of the driver itself: the `data-driver-js`
  build listed in `wasm.sha256`, SHA-256
  `d2ed9e7b89d81f12b877f787a7830b90405357c82c9c28f75f0e048530e91462`.

## Deployment

The chain ID is not part of a contract ID. The same Wasm, nonce and owner give
the same ID on testnet, mainnet and any other chain; the E2E run deploys the
multicall on local chains 0 and 42 and gets the same ID on both. The signer
isn't part of it either. A transaction from one account can deploy with
another account's key as owner, and the ID follows the owner. Whoever deploys
a given Wasm, nonce and owner first takes that ID. For this contract that is
harmless, because with no `init` and no state every deployment at that ID is
the same contract. Check the ID in the deployment receipt before publishing
it.

The Wasm is 52,221 bytes, so the bytecode charge is 5,222,100 gas (100 gas per
byte). The deployment spent 7,044,600 gas on a local Rusk 1.7.1 node, 14.089
DUSK at 2,000 Lux per gas, the minimum deployment price that the Dusk Domains
deploy tooling pays. That tooling's limit, the charge plus 10,000,000, means
holding about 30.4 DUSK up front; unused gas is refunded.

`make wasm` builds the contract, its driver and the test probe into
`target/wasm` with Rust 1.95.0 and the locked dependencies, and strips local
paths from the Wasm. A build from another directory with a fresh Cargo home
gave the same bytes, and CI checks the contract and the driver against
`wasm.sha256`. Rust 1.96.1 fails to link the contract, leaving the host
imports undefined (dusk-network/forge#44), which is why the toolchain is pinned
to 1.95.0.

A node only decodes JSON for a contract whose owner uploaded its driver to that
node. Uploads are per node, and JSON mode trusts the driver each node holds
(see Trust model).

## Using it over HTTP

Rusk reads a request body as bytes, and answers in bytes, only with
`Content-Type: application/octet-stream`. Otherwise it reads the body as text,
decodes it as hex if it starts with `0x`, and answers in hex. The readable way
to drive it by hand is JSON, on a node that has the driver — this reads the
transfer contract's `chain_id`:

```sh
NODE=http://127.0.0.1:18230
MULTICALL=<contract id>

curl -s -X POST "$NODE/on/contracts:$MULTICALL/version" --data ''
# 01000000  (version 1, little-endian)

curl -s -X POST "$NODE/on/contracts:$MULTICALL/aggregate" -H 'Content-Type: application/json' \
  --data '[{"contract":"0100000000000000000000000000000000000000000000000000000000000000","fn_name":"chain_id","fn_args":"","gas_limit":"0"}]'
# {"height":"28","results":[{"Ok":"00"}]}
```

The node decodes only the envelope here: `chain_id`'s reply comes back as the
hex `"00"`, which a client decodes with the transfer contract's driver. The
same batch as raw bytes, for a client that builds the envelope itself:

```sh
# batch.rkyv is the multicall driver's encoding of the call list above.
curl -s -X POST "$NODE/on/contracts:$MULTICALL/aggregate" \
  -H 'Content-Type: application/octet-stream' --data-binary @batch.rkyv --output reply.rkyv
```

A JS client can also run the driver itself. `tests/driver.test.mjs` loads its
Wasm and calls `encode_input_fn` and `decode_output_fn` in about 30 lines.

A batching client such as Dusk Connect would:

1. collect the reads of one tick and encode each with its callee's driver;
2. send one `aggregate` per 256 calls and 64 KiB of envelope, with a cap of 0
   for ordinary reads and an explicit cap for a read known to need more than
   3,000,000,000 gas divided by the batch size plus one;
3. settle each read from its own result, decoding the reply with the callee's
   driver;
4. on HTTP 500, which Rusk 1.7.1 gives without a reason, split the batch or
   fall back to direct queries. A batch whose replies pass 64 KiB fails as a
   whole, and reply sizes aren't known in advance.

## Development

```sh
make wasm   # contract, driver and probe Wasm in target/wasm
make test   # in-VM tests on piecrust 0.31, driver tests in Rust and Node
make lint   # rustfmt, and clippy for the host and wasm32
make e2e    # local Rusk 1.7.1 chains on ports 18230 and 19040
```

`make e2e` needs curl, jq and ss. It fetches the public Rusk 1.7.1 release and
its example genesis, pinned by SHA-256, runs every check on a fresh chain,
deploys again on a second chain, stops its nodes and leaves its evidence in
`e2e/run/chain-*/` (`report.json`, the logs). Set `E2E_HTTP_PORT`,
`E2E_UDP_PORT` or `E2E_RUN_DIR` to change where it runs.

The in-VM tests (`tests/vm`) run the release Wasm against `tests/probe`, a
contract whose views echo, report their caller, height or gas limit, panic,
burn or spin through gas, return large replies or feed data, plus a counter
that writes and a forwarder that calls other contracts. The E2E checks
(`e2e/src/main.rs`) run the same contracts and the genesis transfer and stake
contracts on a real node.
