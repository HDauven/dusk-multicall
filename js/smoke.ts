// A read-only check against testnet, not run in CI. Four balance reads of the
// genesis contracts, made in one task through createMulticallFetch, must go as
// one aggregate query; aggregate() then sends the same reads. Two queries.
//
//   node js/smoke.ts [--record js/test/testnet.json]
//
// `--record` writes the batched query, its envelope and its reply, as the
// fixture the tests replay. DUSK_NODE picks another node.
import assert from 'node:assert/strict'
import { writeFile } from 'node:fs/promises'
import process from 'node:process'
import { aggregate, createMulticallFetch, type Fetch, MULTICALL_CONTRACT_ID } from './src/mod.ts'

const nodeUrl = process.env.DUSK_NODE ?? 'https://testnet.nodes.dusk.network'
const TRANSFER = '01'.padEnd(64, '0')
const STAKE = '02'.padEnd(64, '0')
const bytes = (hex: string) => Uint8Array.from(hex.match(/../g) ?? [], (b) => Number.parseInt(b, 16))
const hex = (u8: Uint8Array) => Array.from(u8, (b) => b.toString(16).padStart(2, '0')).join('')
const u64 = (u8: Uint8Array) => new DataView(u8.buffer, u8.byteOffset, u8.byteLength).getBigUint64(0, true)

// The Lux the transfer contract keeps for three contracts, and the Lux the stake contract burnt.
const reads = [
  { contract: TRANSFER, fnName: 'contract_balance', args: bytes(TRANSFER) },
  { contract: TRANSFER, fnName: 'contract_balance', args: bytes(STAKE) },
  { contract: TRANSFER, fnName: 'contract_balance', args: bytes(MULTICALL_CONTRACT_ID) },
  { contract: STAKE, fnName: 'burnt_amount', args: new Uint8Array() },
]
// The gas policy: a cap for one read, a fair share of the query's gas for the others.
const gasFor = (id: string, fn: string) => (id === STAKE && fn === 'burnt_amount' ? 50_000_000n : 0n)

const sent: { url: string; body: Uint8Array; status: number; reply: Uint8Array }[] = []
const recording: Fetch = async (input, init) => {
  const res = await fetch(input, init)
  const body = new Uint8Array(init?.body as Uint8Array)
  sent.push({ url: String(input), body, status: res.status, reply: new Uint8Array(await res.clone().arrayBuffer()) })
  return res
}

const batched = createMulticallFetch({ nodeUrl, multicallId: MULTICALL_CONTRACT_ID, fetch: recording, gasFor })
const answers = await Promise.all(
  reads.map((r) =>
    batched(`${nodeUrl}/on/contracts:${r.contract}/${r.fnName}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/octet-stream' },
      body: r.args,
    })
  ),
)
assert.equal(sent.length, 1, 'the four reads went as one query')
assert.equal(sent[0].url, `${nodeUrl}/on/contracts:${MULTICALL_CONTRACT_ID}/aggregate`)
for (const [i, res] of answers.entries()) {
  const reply = new Uint8Array(await res.arrayBuffer())
  assert.equal(res.status, 200, `${reads[i].fnName}: ${new TextDecoder().decode(reply)}`)
  assert.equal(reply.length, 8, `${reads[i].fnName} answers a u64`)
  console.log(`${reads[i].fnName}(${reads[i].args.length ? `${hex(reads[i].args).slice(0, 4)}…` : ''}): ${u64(reply)}`)
}

const { height, results } = await aggregate(
  reads.map((r) => ({ ...r, gasLimit: gasFor(r.contract, r.fnName) })),
  { nodeUrl },
)
for (const result of results) assert.ok('Ok' in result && result.Ok.length === 8, JSON.stringify(result))
console.log(`aggregate(): ${results.length} results at height ${height}, ${sent.length + 1} queries in all`)

const out = process.argv.indexOf('--record')
if (out !== -1) {
  const [query] = sent
  const fixture = {
    recorded: `${new URL(nodeUrl).host}, ${new Date().toISOString().slice(0, 10)}: js/smoke.ts, four balance reads of the transfer and stake contracts made in one task, and the aggregate that carried them`,
    reads: reads.map((r) => ({ url: `/on/contracts:${r.contract}/${r.fnName}`, body: hex(r.args) })),
    aggregate: {
      url: new URL(query.url).pathname,
      envelope: hex(query.body),
      status: query.status,
      reply: hex(query.reply),
    },
  }
  await writeFile(process.argv[out + 1], `${JSON.stringify(fixture, null, 2)}\n`)
  console.log(`recorded ${process.argv[out + 1]}`)
}
