// What the tests share: hex, a node that records its requests, an `aggregate`
// reply as the contract writes it, and a wait for an outcome.
import { mock } from 'node:test'
import type { Fetch, MulticallResult } from '../src/mod.ts'

export const NODE = 'https://testnet.nodes.dusk.network'
export const TRANSFER = '01'.padEnd(64, '0')
export const STAKE = '02'.padEnd(64, '0')

export const bytes = (hex: string): Uint8Array<ArrayBuffer> => Uint8Array.from(hex.match(/../g) ?? [], (b) => Number.parseInt(b, 16))
export const hex = (u8: Uint8Array): string => Array.from(u8, (b) => b.toString(16).padStart(2, '0')).join('')

/** A node answering each request with `answer`; `.mock.calls` keeps the requests. */
export function node(answer: (url: string, init: RequestInit) => Response | Promise<Response>) {
  return mock.fn<Fetch>((input, init) => Promise.resolve(answer(String(input), init ?? {})))
}

/** The URL of each request a node got. */
export const urls = (base: ReturnType<typeof node>): string[] => base.mock.calls.map((c) => String(c.arguments[0]))

/** How many calls an `aggregate` envelope carries: its root's length field. */
export function countCalls(body: unknown): number {
  const u8 = new Uint8Array(body as Uint8Array)
  return new DataView(u8.buffer, u8.byteOffset, u8.byteLength).getUint32(u8.length - 4, true)
}

const utf8 = new TextEncoder()
const align = (n: number, to: number) => Math.ceil(n / to) * to
const ERRORS = { OutOfGas: 1, DoesNotExist: 2, Unknown: 3 }

/**
 * An `aggregate` reply as the contract writes it (rkyv 0.7): the bytes of each
 * Ok result and of each panic message too long to sit inline, in call order,
 * then one 16-byte slot per result, then the root.
 */
export function reply(results: MulticallResult[], height = 7n): Uint8Array<ArrayBuffer> {
  const outOfLine = results.map((r) => {
    if ('Ok' in r) return r.Ok
    const message = typeof r.Err === 'object' ? utf8.encode(r.Err.Panic) : null
    return message && message.length > 7 ? message : null
  })
  const slots = align(outOfLine.reduce((n, b) => n + (b?.length ?? 0), 0), 4)
  const root = align(slots + 16 * results.length, 8)
  const out = new Uint8Array(root + 16)
  const view = new DataView(out.buffer)
  let at = 0
  results.forEach((r, i) => {
    const slot = slots + 16 * i
    const extra = outOfLine[i]
    if (extra) out.set(extra, at)
    if ('Ok' in r) {
      view.setInt32(slot + 4, at - (slot + 4), true)
      view.setUint32(slot + 8, r.Ok.length, true)
    } else if (typeof r.Err === 'object') {
      out[slot] = 1
      const message = utf8.encode(r.Err.Panic)
      if (extra) {
        view.setUint32(slot + 8, message.length, true)
        view.setInt32(slot + 12, at - (slot + 8), true)
      } else {
        out.set(message, slot + 8)
        out[slot + 15] = message.length
      }
    } else {
      out[slot] = 1
      out[slot + 4] = ERRORS[r.Err]
    }
    at += extra?.length ?? 0
  })
  view.setBigUint64(root, height, true)
  view.setInt32(root + 8, slots - (root + 8), true)
  view.setUint32(root + 12, results.length, true)
  return out
}

/** Waits until `check` passes, at most `ms`, polling. */
export async function waitFor(check: () => void, ms = 1000): Promise<void> {
  const until = Date.now() + ms
  for (;;) {
    try {
      return check()
    } catch (error) {
      if (Date.now() > until) throw error
    }
    await new Promise((resolve) => setTimeout(resolve, 2))
  }
}
