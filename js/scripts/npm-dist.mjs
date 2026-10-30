// After tsc: the npm build's type files import each other as .js, as its
// JS does, and its docs name the package as npm knows it.
import { readdirSync, readFileSync, writeFileSync } from 'node:fs'

const dist = new URL('../dist/', import.meta.url)

for (const file of readdirSync(dist)) {
  if (!/\.(js|d\.ts)$/.test(file)) continue
  const path = new URL(file, dist)
  const text = readFileSync(path, 'utf8')
    .replace(/(from\s+['"]\.\/[\w-]+)\.ts(['"])/g, '$1.js$2')
    .replaceAll('@hdauven/dusk-multicall', '@heindauven/dusk-multicall')
  writeFileSync(path, text)
}
