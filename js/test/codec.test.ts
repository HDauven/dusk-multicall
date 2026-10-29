import assert from 'node:assert/strict'
import { existsSync } from 'node:fs'
import { readFile } from 'node:fs/promises'
import { test } from 'node:test'
import {
  type Aggregate,
  callBytes,
  decodeAggregate,
  encodeAggregate,
  envelopeLength,
  type MulticallCall,
  type MulticallResult,
  replyLength,
} from '../src/codec.ts'
import { bytes, hex, reply } from './helpers.ts'

/** A reply as the multicall's data driver writes it in JSON. */
const asJson = ({ height, results }: Aggregate) => ({
  height: String(height),
  results: results.map((r) => ('Ok' in r ? { Ok: hex(r.Ok) } : { Err: r.Err })),
})
const fromJson = (calls: { contract: string; fn_name: string; fn_args: string; gas_limit: string }[]) =>
  calls.map((c) => ({ contract: bytes(c.contract), fnName: c.fn_name, args: bytes(c.fn_args), gasLimit: BigInt(c.gas_limit) }))
const toJson = (calls: MulticallCall[]) =>
  calls.map((c) => ({ contract: hex(c.contract), fn_name: c.fnName, fn_args: hex(c.args), gas_limit: String(c.gasLimit) }))

test('encodes and decodes the vectors of the contract\'s own types', async () => {
  // tests/vectors.json holds the rkyv archives of the contract's Rust types.
  const vectors = JSON.parse(await readFile(new URL('../../tests/vectors.json', import.meta.url), 'utf8'))
  const aggregate = vectors.filter((v: { fn: string }) => v.fn === 'aggregate')
  assert.equal(aggregate.length, 3)
  for (const v of aggregate) {
    if (v.kind === 'input') assert.equal(hex(encodeAggregate(fromJson(v.json))), v.rkyv)
    else assert.deepEqual(asJson(decodeAggregate(bytes(v.rkyv), v.json.results.length)), v.json)
  }
})

test('sizes envelopes and replies to the bytes, up to the 64 KiB limits', () => {
  const call = { contract: new Uint8Array(32), fnName: 'contract_balance', args: new Uint8Array(32), gasLimit: 0n }
  assert.equal(encodeAggregate([call, call]).length, envelopeLength(2, 2 * callBytes(call.fnName, call.args)))
  // The largest argument one call can carry, and the largest reply, beside 255 failed calls too.
  assert.equal(envelopeLength(1, 65_472), 65_536)
  assert.equal(replyLength(1, 65_504), 65_536)
  assert.equal(replyLength(256, 61_424), 65_536)
  const results: MulticallResult[] = [{ Ok: bytes('0102') }, { Err: { Panic: 'a message too long to sit inline' } }]
  assert.equal(reply(results).length, replyLength(2, 2 + 32))
})

test('refuses what the contract would refuse', () => {
  const call = { contract: new Uint8Array(32), fnName: 'f', args: new Uint8Array(), gasLimit: 0n }
  assert.equal(encodeAggregate([{ ...call, args: new Uint8Array(65_472) }]).length, 65_536)
  assert.throws(() => encodeAggregate([{ ...call, args: new Uint8Array(65_473) }]), /at most 65536 bytes/)
  assert.throws(() => encodeAggregate(Array(257).fill(call)), /at most 256 calls/)
  assert.throws(() => encodeAggregate([{ ...call, contract: new Uint8Array(31) }]), /32 bytes/)
  for (const gasLimit of [-1n, 1n << 64n, 1 as unknown as bigint]) {
    assert.throws(() => encodeAggregate([{ ...call, gasLimit }]), /u64/)
  }
})

test('refuses a reply that is not exactly the archive of its calls', () => {
  // Two results: 2 bytes of reply and a 27-byte panic message out of line, the
  // slots at 32 and 48, the root at 64.
  const good = reply([{ Ok: bytes('0102') }, { Err: { Panic: 'a message longer than seven' } }])
  assert.equal(decodeAggregate(good, 2).results.length, 2)
  assert.throws(() => decodeAggregate(good, 3), /answered 2 calls, not 3/)
  const edit = (change: (u8: Uint8Array, view: DataView) => void, from = good) => {
    const u8 = from.slice()
    change(u8, new DataView(u8.buffer))
    return u8
  }
  const cases: [string, Uint8Array][] = [
    ['shorter than a root', new Uint8Array(8)],
    ['not 8-aligned', new Uint8Array([...good, 0, 0, 0, 0])],
    ['over 64 KiB', new Uint8Array(65_544)],
    ['a result that is neither Ok nor Err', edit((u8) => (u8[32] = 2))],
    ['an error that is no CallError', edit((u8) => (u8[52] = 4))],
    // A 3-byte reply moved one byte on: it still ends where the results start.
    ['a gap before a reply', edit((_, v) => v.setInt32(8, v.getInt32(8, true) + 1, true), reply([{ Ok: bytes('010203') }, { Err: 'OutOfGas' }]))],
    ['bytes claimed past the results', edit((_, v) => v.setUint32(40, 40, true))],
    ['a panic message claimed twice', edit((_, v) => v.setInt32(60, v.getInt32(60, true) - 2, true))],
    // Here the second slot is at 20, its message's length byte at 35.
    ['an inline message of 8 bytes', edit((u8) => (u8[35] = 8), reply([{ Ok: bytes('0102') }, { Err: { Panic: 'short' } }]))],
    ['a message that is not UTF-8', edit((u8) => (u8[2] = 0xff))],
    ['a message over 200 bytes', reply([{ Ok: bytes('0102') }, { Err: { Panic: 'x'.repeat(201) } }])],
    ['results that end before the root', edit((_, v) => v.setInt32(72, v.getInt32(72, true) + 4, true))],
  ]
  for (const [what, u8] of cases) assert.throws(() => decodeAggregate(u8, 2), /Malformed multicall reply/, what)
})

// The multicall's own data driver: the Wasm `make wasm` builds and each release
// attaches, pinned in wasm.sha256.
const DRIVER = new URL('../../target/wasm/dusk_multicall_driver.wasm', import.meta.url)

test('agrees with the data driver on random envelopes and replies', {
  skip: !existsSync(DRIVER) && 'needs target/wasm/dusk_multicall_driver.wasm (make wasm, or the release asset)',
}, async () => {
  const driver = await loadDriver(DRIVER)
  const rand = random(0x6d756c74)
  const ALPHABET = ['a', 'z', '_', '9', 'é', '€', '𝄞']
  const text = (chars: number) => Array.from({ length: chars }, () => rand.pick(ALPHABET)).join('')
  const gas = () => rand.pick([0n, 1n, BigInt(rand.int(2 ** 32)), (1n << 64n) - 1n, BigInt(rand.int(2 ** 32)) << 32n])
  for (let round = 0; round < 200; round++) {
    const calls = Array.from({ length: round === 0 ? 256 : rand.int(40) }, () => ({
      contract: rand.bytes(32),
      fnName: text(rand.int(12)),
      args: rand.bytes(rand.int(80)),
      gasLimit: gas(),
    }))
    const encoded = encodeAggregate(calls)
    assert.equal(hex(encoded), hex(driver.encode(toJson(calls))), `round ${round}`)

    const results = Array.from({ length: rand.int(40) }, (): MulticallResult => {
      const kind = rand.int(5)
      if (kind === 0) return { Ok: rand.bytes(rand.int(64)) }
      if (kind === 1) return { Err: { Panic: [...text(rand.int(60))].slice(0, 50).join('') } }
      return { Err: rand.pick(['OutOfGas', 'DoesNotExist', 'Unknown'] as const) }
    })
    const archive = reply(results, BigInt(rand.int(2 ** 32)) << BigInt(rand.int(33)))
    const decoded = asJson(decodeAggregate(archive, results.length))
    assert.deepEqual(decoded, driver.decode(archive), `round ${round}`)
    // A reply the codec takes after a byte changed, the driver reads the same way.
    for (let flip = 0; flip < 8; flip++) {
      const changed = archive.slice()
      changed[rand.int(changed.length)] ^= 1 << rand.int(8)
      let mine
      try {
        mine = asJson(decodeAggregate(changed, results.length))
      } catch {
        continue
      }
      assert.deepEqual(mine, driver.decode(changed), `round ${round}, flip ${flip}`)
    }
  }
})

/** The driver's `aggregate` encoder and decoder, through the exports a JS client calls. */
async function loadDriver(path: URL) {
  const { instance } = await WebAssembly.instantiate(new Uint8Array(await readFile(path)), {})
  // deno-lint-ignore no-explicit-any
  const ffi = instance.exports as any
  ffi.init()
  const OUT = 1 << 20
  const run = (exported: (...args: number[]) => number, input: Uint8Array): Uint8Array => {
    const put = (data: Uint8Array) => {
      const ptr = ffi.alloc(data.length)
      new Uint8Array(ffi.memory.buffer, ptr, data.length).set(data)
      return [ptr, data.length]
    }
    const args = [...put(new TextEncoder().encode('aggregate')), ...put(input)]
    const out = ffi.alloc(OUT)
    const code = exported(...args, out, OUT)
    if (code !== 0) ffi.get_last_error(out, OUT)
    const length = new DataView(ffi.memory.buffer).getUint32(out, true)
    const output = new Uint8Array(ffi.memory.buffer, out + 4, length).slice()
    ffi.dealloc(args[0], args[1])
    ffi.dealloc(args[2], args[3])
    ffi.dealloc(out, OUT)
    if (code !== 0) throw new Error(new TextDecoder().decode(output))
    return output
  }
  return {
    encode: (json: unknown) => run(ffi.encode_input_fn, new TextEncoder().encode(JSON.stringify(json))),
    decode: (archive: Uint8Array) => JSON.parse(new TextDecoder().decode(run(ffi.decode_output_fn, archive))),
  }
}

/** A seeded generator (mulberry32), so a failing round can be run again. */
function random(seed: number) {
  let state = seed >>> 0
  const next = () => {
    state = (state + 0x6d2b79f5) >>> 0
    let t = state
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
  const int = (n: number) => Math.floor(next() * n)
  return {
    int,
    bytes: (n: number) => Uint8Array.from({ length: n }, () => int(256)),
    pick: <T>(list: readonly T[]): T => list[int(list.length)],
  }
}
