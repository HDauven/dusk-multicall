#!/usr/bin/env bash
# Runs the end-to-end checks on fresh local Rusk 1.7.1 chains: builds the
# Wasm, fetches the public release and its example genesis, runs every check
# on chain 0, deploys the multicall again on chain 42 to compare its ID, and
# stops each node it started.
set -euo pipefail
cd "$(dirname "$0")/.."
make wasm
http_port=${E2E_HTTP_PORT:-18230}
udp_port=${E2E_UDP_PORT:-19040}
run=$(realpath -m "${E2E_RUN_DIR:-e2e/run}")
tag=dusk-rusk-1.7.1
release=rusk-1.7.1-linux-x64-archive

for port in "$http_port/tcp" "$udp_port/udp"; do
  if ss -Hln "( sport = :${port%/*} )" --"${port#*/}" | grep -q .; then
    echo "port $port is in use" >&2
    exit 1
  fi
done

mkdir -p "$run/rusk/examples"
fetch() { [[ -s $2 ]] || curl --fail --silent --show-error --location --retry 3 "$1" --output "$2"; }
fetch "https://github.com/dusk-network/rusk/releases/download/$tag/$release.tar.gz" "$run/rusk.tar.gz"
tar -xOzf "$run/rusk.tar.gz" "$release/rusk" > "$run/rusk/rusk"
for file in genesis.toml consensus.keys recovery-phrase.txt recovery-phrase-unstaked.txt; do
  fetch "https://raw.githubusercontent.com/dusk-network/rusk/$tag/examples/$file" "$run/rusk/examples/$file"
done
(cd "$run/rusk" && sha256sum --check --quiet) <<'SUMS'
716517e7a00de47bf4a65af06c8ff46c9837d90e278612c4466be144e3411363  rusk
b5c5e64494acd7e4f43c9bcc66655983f4e8dbc7f482f99269fbf8bb44362adb  examples/genesis.toml
6f6bb009e9f1825f6ac8fc539a5cbc11257c92f4af00fa62debefaba4c213a65  examples/consensus.keys
3f4231b2a63ea5851382f59f8707cee40c87ab27fd331fafc0667af8c22c594e  examples/recovery-phrase.txt
98f38c1ffefdca4891e5ccdc5ed44af57fb23f9dbb7b41ed6385d891943f00d9  examples/recovery-phrase-unstaked.txt
SUMS
chmod +x "$run/rusk/rusk"

node_pid=''
stop() {
  if [[ -n $node_pid ]]; then
    kill "$node_pid" 2>/dev/null || true
    wait "$node_pid" 2>/dev/null || true
    node_pid=''
  fi
}
trap stop EXIT

# Starts a node on a fresh chain with network ID $1, in a new directory $chain.
start() {
  chain=$(mktemp -d "$run/chain-XXXXXX")
  mkdir -p "$chain/profile" "$chain/tmp"
  local rusk=(env TMPDIR="$chain/tmp" RUSK_PROFILE_PATH="$chain/profile" "$run/rusk/rusk")
  "${rusk[@]}" recovery state --init "$run/rusk/examples/genesis.toml" \
    --output "$chain/genesis.state" > "$chain/genesis.log" 2>&1
  # The block gas limit, which Rusk also gives each query, and Phoenix off
  # as on mainnet after activation (Dusk Domains' CI node does the same).
  printf '[vm]\nblock_gas_limit = 3000000000\n[vm.features]\nDISABLE_PHOENIX = 1\n' > "$chain/node.toml"
  cp "$run/rusk/examples/consensus.keys" "$chain/profile/consensus.keys"
  RUSK_MINIMUM_BLOCK_TIME=1 RUSK_ALLOW_CFG_OVERRIDE=true DUSK_CONSENSUS_KEYS_PASS=password \
    "${rusk[@]}" --config "$chain/node.toml" --state "$chain/genesis.state" --network-id "$1" \
    --consensus-keys-path "$chain/profile/consensus.keys" --http-listen-addr "127.0.0.1:$http_port" \
    --kadcast-public-address "127.0.0.1:$udp_port" --kadcast-listen-address "127.0.0.1:$udp_port" \
    > "$chain/node.log" 2>&1 &
  node_pid=$!
  echo "$node_pid" > "$chain/node.pid"
  for _ in $(seq 120); do
    kill -0 "$node_pid"
    if curl --fail --silent "http://127.0.0.1:$http_port/on/graphql/query" \
      --data '{block(height:-1){header{height}}}' | grep -q '"height":[1-9]'; then
      return
    fi
    sleep 1
  done
  echo "the node did not start; see $chain/node.log" >&2
  exit 1
}

e2e() {
  cargo run --locked --release -p multicall-e2e -- --node "http://127.0.0.1:$http_port/" \
    --rusk-dir "$run/rusk" --wasm target/wasm --out "$chain" --node-log "$chain/node.log" "$@" \
    2>&1 | tee "$chain/e2e.log"
}

start 0
e2e
first=$chain
stop
start 42
e2e --deploy-only
stop
ids=$(jq -r .multicall "$first/report.json" "$chain/report.json" | sort -u)
echo "multicall ID on chains 0 and 42: $ids"
[[ $(wc -l <<< "$ids") == 1 ]]
