// The publish workflow's last step, run as GitHub runs it, with stand-ins for
// deno, curl and sleep: a version JSR already has passes only with provenance.
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmdirSync, unlinkSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'

const workflow = readFileSync(new URL('../../.github/workflows/publish-js.yml', import.meta.url), 'utf8')
// The step's script: the block under its `run: |`, up to the next step.
const start = workflow.indexOf('run: |', workflow.indexOf('- name: Publish to JSR with provenance'))
const next = workflow.indexOf('\n      - ', start)
const block = workflow.slice(start, next === -1 ? undefined : next)
const script = block.split('\n').slice(1).map((line) => line.slice(10)).join('\n')

type Attempt = { out: string; code: number }
const PUBLISHED = { out: 'Successfully published @hdauven/dusk-multicall@0.1.0', code: 0 }
const SKIPPED = { out: 'Skipping, already published @hdauven/dusk-multicall@0.1.0', code: 0 }
const POLL_FAILED = { out: 'error: Failed to get publishing status', code: 1 }
const SIGNED = { version: '0.1.0', rekorLogId: '3547150194' }
const UNSIGNED = { version: '0.1.0', rekorLogId: null }

/** The step's exit code, with deno answering `attempts` in turn and JSR's API `version`, or failing without one. */
function publish(attempts: Attempt[], version?: object): number | null {
  const dir = mkdtempSync(join(tmpdir(), 'publish-js-'))
  const bin = join(dir, 'bin')
  const files = ['jsr.json', 'attempts.json', 'version.json', 'n', 'publish.log', 'bin/deno', 'bin/curl', 'bin/sleep']
  mkdirSync(bin)
  writeFileSync(join(dir, 'jsr.json'), JSON.stringify({ name: '@hdauven/dusk-multicall', version: '0.1.0' }))
  writeFileSync(join(dir, 'attempts.json'), JSON.stringify(attempts))
  if (version) writeFileSync(join(dir, 'version.json'), JSON.stringify(version))
  const stub = (name: string, body: string) => {
    writeFileSync(join(bin, name), body)
    chmodSync(join(bin, name), 0o755)
  }
  stub('deno', `#!/usr/bin/env node
const fs = require('fs')
const n = fs.existsSync('n') ? Number(fs.readFileSync('n', 'utf8')) : 0
fs.writeFileSync('n', String(n + 1))
const { out, code } = JSON.parse(fs.readFileSync('attempts.json', 'utf8'))[n]
console.log(out)
process.exit(code)
`)
  stub('curl', `#!/bin/sh
case "$*" in *https://api.jsr.io/scopes/hdauven/packages/dusk-multicall/versions/0.1.0*) [ -f version.json ] && exec cat version.json;; esac
exit 22
`)
  stub('sleep', '#!/bin/sh\n')
  try {
    return spawnSync('bash', ['--noprofile', '--norc', '-eo', 'pipefail', '-c', script], {
      cwd: dir,
      env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, RUNNER_TEMP: dir },
      stdio: 'ignore',
    }).status
  } finally {
    for (const file of files) if (existsSync(join(dir, file))) unlinkSync(join(dir, file))
    rmdirSync(bin)
    rmdirSync(dir)
  }
}

test('the JS CI runs when the workflow its test runs changes', () => {
  const ci = readFileSync(new URL('../../.github/workflows/js.yml', import.meta.url), 'utf8')
  const filters = [...ci.matchAll(/^ +paths: \[(.*)\]$/gm)].map((match) => match[1])
  assert.equal(filters.length, 2)
  for (const paths of filters) assert.ok(paths.includes("'.github/workflows/publish-js.yml'"), paths)
})

test('the publish step passes a version only with its provenance', () => {
  assert.equal(publish([PUBLISHED]), 0)
  // A run after JSR finished a queued publish that no attempt signed.
  assert.equal(publish([SKIPPED], UNSIGNED), 1)
  assert.equal(publish([SKIPPED], SIGNED), 0)
  assert.equal(publish([SKIPPED]), 1)
  // A retry that finds the queued task finished, unsigned, or signs it.
  assert.equal(publish([POLL_FAILED, SKIPPED], UNSIGNED), 1)
  assert.equal(publish([POLL_FAILED, PUBLISHED]), 0)
  // Any other failure is not retried.
  assert.equal(publish([{ out: 'error: Unauthorized', code: 1 }, PUBLISHED]), 1)
})
