# Dusk Multicall

One contract query that makes many contract reads, all at one state. `aggregate`
takes a list of calls and returns the block height with each call's raw reply
or error, in call order. A failing call never stops the others. The contract
holds no state, its owner has no on-chain powers over it, it can't be upgraded,
and it refuses to run unless a query calls it directly.

A page that reads 40 values sends 40 queries. Through the multicall it sends
one, and the 40 values come back from one block, so they agree with each
other. Queries pay no fee; gas is only a per-query bound, so batching costs
the user nothing.

## Performance

The more reads a page makes, the more it saves. One request and one session
replace one per read, and the node spends a fraction of the CPU per read. On a
local Rusk 1.7.1 node, one client loading a DRC-20 token page (its name,
symbol, decimals and total supply, plus the balance of each holder):

| Holders | One query per read | One `aggregate` | Faster | Node CPU per read |
| ---: | ---: | ---: | ---: | ---: |
| 10 | 24.2 ms | 3.0 ms | 8× | 1,369 → 189 µs |
| 32 | 61.6 ms | 5.7 ms | 11× | 1,333 → 149 µs |
| 64 | 122 ms | 9.2 ms | 13× | 1,450 → 145 µs |
| 128 | 226 ms | 15.0 ms | 15× | 1,389 → 124 µs |
| 251 | 387 ms | 26.2 ms | 15× | 1,328 → 113 µs |

With 8 clients at once, the node served 1,869 reads per second as single
queries and 11,082 through one `aggregate` per page of 251 holders. Small views
of the genesis contracts gave the same picture: 4× faster at 10 reads, 25× at
256. The multicall's own cost is about 4,600 gas per call, so 256 reads use
0.04% of a query's gas bound, and every batched read returned the same bytes
as the single query. Times are medians of six runs on one machine; the ratios
matter more than the milliseconds.

## When to use it

Use it for independent reads a client would otherwise send one by one:

- a client library that batches every contract read made in one tick into one
  query, as viem's `batch.multicall` does;
- a page load;
- reads that must agree with each other, such as a price and the config it
  depends on, read before signing.

A purpose-built aggregator contract is the better tool when a read depends on
another read's result (the multicall can't feed one reply into the next call's
argument), or when the answer should be compact and already checked. The
multicall returns every raw reply.

## Using it from a client

Use [w3sper](https://jsr.io/@dusk/w3sper) and run every data driver on the
client: the multicall's driver for the envelope and each callee's driver for
its own call. Load the drivers from local copies whose SHA-256 you pinned, not
from a node. The node then only sees and returns bytes, and every value is
decoded by a driver the client chose. The example below runs unchanged with
w3sper 1.7.0-rc.0 and 1.6.0 on Deno, against Rusk 1.7.1.

A client encodes each inner call's argument with **that callee's** driver,
wraps the calls in one envelope with the **multicall's** driver, sends one
query, then decodes each inner reply with the callee's driver again. That works
for every contract with a `data-driver-js` Wasm, which Forge builds for its
contracts and the genesis transfer and stake contracts ship. For a contract
without one,
encode the argument by hand as the rkyv 0.7 archive (`size_32`) of the
function's argument type, for example with the contract's own Rust types, and
decode the reply bytes the same way. This is the same shape as Ethereum's
Multicall3, where each inner call carries ABI-encoded calldata and viem's
`multicall` encodes and decodes it for you.

This reads a DRC-20 token, the `drc20_roles_pausable` example in
[dusk-network/contracts](https://github.com/dusk-network/contracts/tree/main/standards/examples/drc20_roles_pausable),
in one query: its name, symbol, decimals and total supply, and the balance of
every holder in `HOLDERS`.

```js
import { Contract, Network } from '@dusk/w3sper'

const network = await Network.connect(NODE_URL)

// Drivers from local files with pinned hashes, never fetched from a node.
// `local(path)` returns the file's bytes after checking its SHA-256;
// `hex` and `unhex` convert between bytes and hex strings.
const multicall = new Contract({
  contractId: MULTICALL,
  driver: await network.dataDrivers.get(MULTICALL, await local('dusk_multicall_driver.wasm')),
  network,
})
// The token's driver is its `make wasm-dd` build, not the contract Wasm.
const token = await network.dataDrivers.get(TOKEN, await local('drc20_roles_pausable_driver.wasm'))

// 1. Encode each call's argument with the token's driver.
const read = (fn_name, args = null) => ({
  contract: TOKEN,
  fn_name,
  fn_args: hex(token.encodeInputFn(fn_name, JSON.stringify(args))),
  gas_limit: '0', // a fair share of the query's gas
})
const calls = [
  read('name'),
  read('symbol'),
  read('decimals'),
  read('total_supply'),
  ...HOLDERS.map((address) => read('balance_of', { account: { Moonlight: address } })),
]

// 2. One query. w3sper encodes the envelope with the multicall's driver, sends
//    it as bytes and decodes the reply with the same driver.
const { height, results } = await multicall.call.aggregate(calls)

// 3. Decode each reply with the token's driver. Test the discriminant, not the
//    payload: an empty `Ok` is a valid reply (a function that returns `()`).
const values = results.map((result, i) =>
  Object.hasOwn(result, 'Ok')
    ? token.decodeOutputFn(calls[i].fn_name, unhex(result.Ok))
    : { error: result.Err },
)
// Every value was read at block `height`. The token's driver returns u64
// values as decimal strings and `decimals` as a number.
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

Encoding an argument can cost more than the read. The DRC-20 driver takes
about 1.5 ms per `balance_of`, because it validates the holder's address as a
BLS key, so 251 holders spend about 400 ms in the client. A client that reads
the same accounts again keeps each encoded `fn_args` and reuses it; the
w3sper path then loads 128 holders in 14 ms, as fast as raw bytes.

A node can decode JSON too, but only the envelope: it holds the multicall's
driver, not each callee's, so the inner replies stay hex. Running the drivers
locally, as above, needs nothing from the node and decodes every value.

## ABI

| Export | Argument | Reply |
| --- | --- | --- |
| `aggregate` | `Vec<Call>` | `Aggregate` |
| `version` | ignored | `1u32` |

Both run only as direct queries. The Wasm exports are written by hand. Dusk
Forge (`#[dusk_forge::contract]`) generates the envelope's data driver, the
`data-driver-js` build, from the contract's methods.

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

1. Decodes the argument into an owned `Vec<Call>`. An argument that isn't a
   valid archive is refused here, before the caller is checked.
2. Refuses with `multicall: direct queries only` if any contract called it
   (`abi::caller()` is not `None`). On Rusk 1.7.1 every transaction reaches its
   contract through the transfer contract's `spend_and_execute`, so this
   refuses every transaction, every contract caller and every nested multicall.
   Only a query calls a contract with no caller.
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

Each of these fails the whole query, and the node answers with an HTTP error
instead of a reply. Every other failure belongs to one call and comes back in
`results`.

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
| a lone call | 1,499,997,445 | 1,500,000,000 |
| 1st to 4th of 4 cheap calls | 599,998,003, 749,996,199, 999,993,897, 1,499,989,414 | 600,000,000 for the first |
| 6th of 7, after 5 failures | 374,997,238 | 375,000,000 |
| 51st of 52, after 50 failures | 56,599,684 | 56,603,773 |
| 255th of 256, after 254 failures | 11,667,842 | 11,673,151 |

The shares of cheap calls grow, because each leaves most of its share unused.
After 5, 50 and 255 failures a cheap call still succeeds.

A call that needs more than its share needs an explicit cap. In a 256-call
batch a share is about 11.7 million gas, and a lone call's is 1.5 billion.
The probe's `spin(1,000,000)` needs 50 million, so in a full batch it runs out
of gas at cap 0 and succeeds with a cap of 200 million, and the other 255
calls succeed either way.

The share kept back covers the largest reply. The worst case is a full batch
whose calls fail at their shares, the last one included. On the node, 255
failures left 23,334,726 gas, the last call's share was 11,667,449, and a
failing last call leaves 11,667,277 for the encoding, while a full 64 KiB of
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
| `aggregate([])` | 3,629 |
| `aggregate([echo; 256])` | 1,188,120 (4,623 per call) |
| each reply byte | about 70 |
| setup before the first call | 6,942 |
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
  read in one session at one state root. The node you query is the trust root
  for both, as for any query.
- **It can only read.** The Wasm imports six host functions (`caller`, `c`,
  `hd`, `limit`, `spent` and `panic`), none that emit events, feed data or
  move value. It has no `init` and no state. The owner has no on-chain powers:
  no transaction can change the code, and there is nothing to administer.
- **Pin the drivers too.** Pinning the contract does not pin a data driver,
  so a client runs its own pinned copies (see Using it from a client). The
  multicall's is the `data-driver-js` build listed in `wasm.sha256`, SHA-256
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

The Wasm is 50,745 bytes, so the bytecode charge is 5,074,500 gas (100 gas per
byte). The deployment spent 6,897,000 gas on a local Rusk 1.7.1 node, 13.794
DUSK at 2,000 Lux per gas, Rusk's default minimum gas price for deployments.
A gas limit of the charge plus 10,000,000 means holding about 30.1 DUSK up
front; unused gas is refunded.

`make wasm` builds the contract, its driver and the test probe into
`target/wasm` with Rust 1.95.0 and the locked dependencies, and strips local
paths from the Wasm. A build from another directory with a fresh Cargo home
gave the same bytes, and CI checks the contract and the driver against
`wasm.sha256`. Rust 1.96.1 fails to link the contract, leaving the host
imports undefined (dusk-network/forge#44), which is why the toolchain is pinned
to 1.95.0.

A client that runs the drivers locally needs no driver on the node. A node
only answers JSON for a contract whose owner uploaded its driver to that node.

## Using it over HTTP

Rusk reads a request body as bytes, and answers in bytes, only with
`Content-Type: application/octet-stream`. Otherwise it reads the body as text,
decodes it as hex if it starts with `0x`, and answers in hex. To try it by
hand, JSON works on a node that holds the driver. This reads the transfer
contract's `chain_id`:

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

`tests/driver.test.mjs` runs the driver Wasm directly, without w3sper, in
about 30 lines.

A batching client such as Dusk Connect would:

1. collect the reads of one tick and encode each with its callee's driver;
2. send one `aggregate` per 256 calls and 64 KiB of envelope, with a cap of 0
   for ordinary reads and an explicit cap for a read known to need more than
   3,000,000,000 gas divided by the batch size plus one;
3. settle each read from its own result, decoding the reply with the callee's
   driver;
4. when the whole query fails, split the batch or fall back to direct
   queries. A batch whose replies pass 64 KiB fails as a whole, and reply
   sizes aren't known in advance.

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
