// The JS data driver (target/wasm/dusk_multicall_driver.wasm, from
// `make wasm`) against the golden vectors, through the Wasm exports a JS
// client calls. Run with `node --test tests/driver.test.mjs`.
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const read = path => readFile(new URL(path, import.meta.url))
const { instance } = await WebAssembly.instantiate(await read('../target/wasm/dusk_multicall_driver.wasm'), {})
const driver = instance.exports
driver.init()

const OUT = 1 << 20
const text = new TextEncoder()
const hex = bytes => Buffer.from(bytes).toString('hex')

// Calls a driver export with (name, input) and returns its output bytes.
function run (exported, name, input) {
  const put = bytes => {
    const ptr = driver.alloc(bytes.length)
    new Uint8Array(driver.memory.buffer, ptr, bytes.length).set(bytes)
    return [ptr, bytes.length]
  }
  const args = [...put(text.encode(name)), ...put(input)]
  const out = driver.alloc(OUT)
  const code = exported(...args, out, OUT)
  if (code !== 0) driver.get_last_error(out, OUT)
  const length = new DataView(driver.memory.buffer).getUint32(out, true)
  const bytes = new Uint8Array(driver.memory.buffer, out + 4, length).slice()
  driver.dealloc(args[0], args[1])
  driver.dealloc(args[2], args[3])
  driver.dealloc(out, OUT)
  if (code !== 0) throw new Error(new TextDecoder().decode(bytes))
  return bytes
}
const encodeInput = (name, json) => run(driver.encode_input_fn, name, text.encode(JSON.stringify(json)))
const decode = (exported, name, bytes) => JSON.parse(new TextDecoder().decode(run(exported, name, bytes)))

test('the Wasm driver reproduces every vector', async () => {
  for (const v of JSON.parse(await read('./vectors.json'))) {
    const bytes = Buffer.from(v.rkyv, 'hex')
    if (v.kind === 'input') {
      assert.equal(hex(encodeInput(v.fn, v.json)), v.rkyv, v.fn)
      assert.deepEqual(decode(driver.decode_input_fn, v.fn, bytes), v.json, v.fn)
    } else {
      assert.deepEqual(decode(driver.decode_output_fn, v.fn, bytes), v.json, v.fn)
    }
  }
})

test('the Wasm driver refuses what the contract would', () => {
  const call = { contract: 'aa'.repeat(32), fn_name: 'f', fn_args: '', gas_limit: '0' }
  assert.equal(hex(encodeInput('aggregate', [call])), hex(encodeInput('aggregate', [{ ...call, gas_limit: 0 }])))
  assert.throws(() => encodeInput('aggregate', [{ ...call, allow_failure: true }]), /unknown field/)
  assert.throws(() => encodeInput('aggregate', [{ ...call, contract: 'aa' }]), /Json/)
  assert.throws(() => encodeInput('transfer', null), /unknown fn transfer/)
})

test('the README walk-through decodes every Ok, an empty one included', async () => {
  const v = JSON.parse(await read('./vectors.json')).find(v => v.fn === 'aggregate' && v.kind === 'output')
  const { results } = decode(driver.decode_output_fn, 'aggregate', Buffer.from(v.rkyv, 'hex'))
  // As in the README: test the discriminant, then always decode the bytes.
  const settle = (result, d, fn) =>
    Object.hasOwn(result, 'Ok') ? d.decode(fn, Buffer.from(result.Ok, 'hex')) : { error: result.Err }
  const seen = []
  const callee = { decode: (fn, bytes) => (seen.push(bytes.length), { fn, length: bytes.length }) }
  const settled = results.map(result => settle(result, callee, 'view'))
  assert.deepEqual(seen, [2, 0])
  assert.deepEqual(settled.at(-1), { fn: 'view', length: 0 })
  assert.deepEqual(settled[1], { error: { Panic: 'boom' } })
  // Empty bytes are a real value: a unit decodes to null.
  assert.equal(decode(driver.decode_input_fn, 'version', new Uint8Array()), null)
})
