// MIT License
//
// Copyright (c) 2026 Aurora Dauven
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

/**
 * The envelope and the reply of Multicall for Dusk's `aggregate`, encoded and
 * decoded by hand as the rkyv 0.7 archives (`size_32`) the contract reads and
 * writes, so a client needs no data driver for them. Each call's argument and
 * reply stay in the callee's own encoding.
 *
 * @example Two reads of the transfer contract in one envelope
 * ```ts
 * import { decodeAggregate, encodeAggregate, MULTICALL_CONTRACT_ID } from '@hdauven/dusk-multicall/codec'
 *
 * const transfer = new Uint8Array(32)
 * transfer[0] = 1
 * const calls = [
 *   { contract: transfer, fnName: 'num_notes', args: new Uint8Array(), gasLimit: 0n },
 *   { contract: transfer, fnName: 'root', args: new Uint8Array(), gasLimit: 0n },
 * ]
 * const node = 'https://testnet.nodes.dusk.network'
 * const res = await fetch(`${node}/on/contracts:${MULTICALL_CONTRACT_ID}/aggregate`, {
 *   method: 'POST',
 *   headers: { 'Content-Type': 'application/octet-stream' },
 *   body: encodeAggregate(calls),
 * })
 * const { height, results } = decodeAggregate(new Uint8Array(await res.arrayBuffer()), calls.length)
 * ```
 * @module
 */

/** Multicall for Dusk's contract ID on mainnet and testnet: the `v0.1.0` contract, nonce 0, one owner. */
export const MULTICALL_CONTRACT_ID = '93de287b7d349d678a0038fe7772e8960e1a84a1b5da2d0b1f62f532616ce1d4'
/** The most calls one `aggregate` takes. */
export const MAX_CALLS = 256
/** The most bytes an `aggregate` envelope, or its reply, may take: 64 KiB. */
export const ARGBUF_LEN = 65536
/** The most bytes of a callee's panic message a result keeps. */
export const MAX_PANIC_BYTES = 200
/** rkyv 0.7 keeps a string of at most this many bytes inside its 8-byte slot (`size_32`). */
const INLINE = 7
/** `ArchivedCall`: contract, name, argument, gas limit; aligned to 8. */
const CALL = 56
/** `ArchivedResult<ArchivedVec<u8>, ArchivedCallError>`: aligned to 4. */
const SLOT = 16
/** `ArchivedAggregate`: height, then the results; aligned to 8. */
const ROOT = 16

/** One call in an `aggregate` envelope. */
export interface MulticallCall {
  /** The callee's contract ID, 32 bytes. */
  contract: Uint8Array
  /** The function to call. */
  fnName: string
  /** The argument in the callee's own encoding. */
  args: Uint8Array
  /** `0n` for a fair share of the query's gas, else the call's limit. */
  gasLimit: bigint
}
/**
 * Why a call failed: the callee panicked, with its message cut to
 * {@linkcode MAX_PANIC_BYTES} bytes, ran out of gas, doesn't exist, or failed
 * otherwise, such as a missing function.
 */
export type CallError = { Panic: string } | 'OutOfGas' | 'DoesNotExist' | 'Unknown'
/** A call's result: exactly the bytes the callee returned, or why it failed. */
export type MulticallResult = { Ok: Uint8Array } | { Err: CallError }
/** The `aggregate` reply. */
export interface Aggregate {
  /** The height of the block whose state the query read. */
  height: bigint
  /** One result per call, in call order. */
  results: MulticallResult[]
}

const align = (n: number, to: number) => Math.ceil(n / to) * to
const utf8 = new TextEncoder()
/** The bytes a call's name, when not inline, and its argument add to an envelope beside its slot. */
export function callBytes(fnName: string, args: Uint8Array): number {
  const name = utf8.encode(fnName).length
  return (name > INLINE ? name : 0) + args.length
}
/** The size of the envelope of `calls` calls whose names and arguments add `bytes` ({@linkcode callBytes}). */
export function envelopeLength(calls: number, bytes: number): number {
  return align(bytes, 8) + CALL * calls + 8
}
/**
 * The size of the reply to `calls` calls whose replies and long panic messages add `outOfLine`
 * bytes: what the multicall checks against its 64 KiB bound after every call.
 */
export function replyLength(calls: number, outOfLine: number): number {
  return align(align(outOfLine, 4) + SLOT * calls, 8) + ROOT
}

/**
 * `Vec<Call>` as the multicall's `aggregate` reads it. Throws for more than
 * {@linkcode MAX_CALLS} calls, an envelope over {@linkcode ARGBUF_LEN} bytes, a
 * contract ID that isn't 32 bytes or a gas limit that isn't a `u64`.
 */
export function encodeAggregate(calls: readonly MulticallCall[]): Uint8Array {
  if (calls.length > MAX_CALLS) throw new Error(`A multicall takes at most ${MAX_CALLS} calls`)
  const names = calls.map((c) => utf8.encode(c.fnName))
  let data = 0
  for (const c of calls) {
    if (c.contract.length !== 32) throw new Error('A multicall callee is 32 bytes')
    if (typeof c.gasLimit !== 'bigint' || c.gasLimit < 0n || c.gasLimit >= 1n << 64n)
      throw new Error('A multicall gas limit is a u64')
    data += callBytes(c.fnName, c.args)
  }
  const size = envelopeLength(calls.length, data)
  if (size > ARGBUF_LEN) throw new Error(`A multicall envelope takes at most ${ARGBUF_LEN} bytes`)
  const out = new Uint8Array(size),
    view = new DataView(out.buffer)
  // rkyv writes what each call points to first, in call order: its name when not inline, then its argument.
  const at: { name: number; args: number }[] = []
  let pos = 0
  for (const [i, c] of calls.entries()) {
    const name = pos
    if (names[i].length > INLINE) {
      out.set(names[i], pos)
      pos += names[i].length
    }
    at.push({ name, args: pos })
    out.set(c.args, pos)
    pos += c.args.length
  }
  const array = align(pos, 8)
  for (const [i, c] of calls.entries()) {
    const slot = array + i * CALL
    out.set(c.contract, slot)
    const name = names[i]
    if (name.length > INLINE) {
      view.setUint32(slot + 32, name.length, true)
      view.setInt32(slot + 36, at[i].name - (slot + 32), true)
    } else {
      out.set(name, slot + 32)
      out[slot + 39] = name.length
    }
    view.setInt32(slot + 40, at[i].args - (slot + 40), true)
    view.setUint32(slot + 44, c.args.length, true)
    view.setBigUint64(slot + 48, c.gasLimit, true)
  }
  const root = array + CALL * calls.length
  view.setInt32(root, array - root, true)
  view.setUint32(root + 4, calls.length, true)
  return out
}

// Rust's `String` keeps a leading U+FEFF: so does a panic message read here.
const fatal = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true })
/**
 * The multicall's reply: the height its query read at and one result per call, in call order. Anything
 * that is not exactly such an archive of `expected` results, as rkyv writes it, throws: each result's
 * bytes out of line follow the previous result's from the start of the reply, with no gap and nothing
 * claimed twice; the results start at the 4-aligned end of those bytes, and the root at theirs, 8-aligned.
 */
export function decodeAggregate(bytes: Uint8Array, expected: number): Aggregate {
  const malformed = () => new Error('Malformed multicall reply')
  if (bytes.length < ROOT || bytes.length % 8 !== 0 || bytes.length > ARGBUF_LEN) throw malformed()
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength),
    root = bytes.length - ROOT,
    height = view.getBigUint64(root, true),
    count = view.getUint32(root + 12, true)
  if (count !== expected) throw new Error(`The multicall answered ${count} calls, not ${expected}`)
  const slots = root + 8 + view.getInt32(root + 8, true)
  if (slots < 0 || slots % 4 !== 0 || align(slots + SLOT * count, 8) !== root) throw malformed()
  // The bytes the archived value at `base` points to, its offset from `base` stored at `at`: right after
  // the previous result's, and before the results.
  let next = 0
  const claim = (base: number, at: number, length: number): Uint8Array => {
    const start = base + view.getInt32(at, true)
    if (start !== next || start + length > slots) throw malformed()
    next = start + length
    return bytes.subarray(start, next)
  }
  const results: MulticallResult[] = []
  for (let i = 0; i < count; i++) {
    const slot = slots + i * SLOT
    const tag = bytes[slot]
    if (tag === 0) {
      results.push({ Ok: claim(slot + 4, slot + 4, view.getUint32(slot + 8, true)).slice() })
      continue
    }
    if (tag !== 1) throw malformed()
    const error = bytes[slot + 4]
    if (error === 1) results.push({ Err: 'OutOfGas' })
    else if (error === 2) results.push({ Err: 'DoesNotExist' })
    else if (error === 3) results.push({ Err: 'Unknown' })
    else if (error === 0) {
      const repr = slot + 8,
        last = bytes[repr + 7],
        length = (last & 0x80) === 0 ? last : view.getUint32(repr, true)
      // rkyv keeps a string of 7 bytes or fewer inline, and only a longer one out of line.
      if ((last & 0x80) === 0 ? length > INLINE : length <= INLINE) throw malformed()
      const text = (last & 0x80) === 0 ? bytes.subarray(repr, repr + length) : claim(repr, repr + 4, length)
      if (text.length > MAX_PANIC_BYTES) throw malformed()
      let message: string
      try {
        message = fatal.decode(text)
      } catch {
        throw malformed()
      }
      results.push({ Err: { Panic: message } })
    } else throw malformed()
  }
  if (align(next, 4) !== slots) throw malformed()
  return { height, results }
}
