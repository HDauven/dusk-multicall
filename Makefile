# The Wasm builds strip local paths from panic locations, so the pinned
# toolchain and Cargo.lock give the same bytes in any checkout and Cargo home.
CARGO_HOME ?= $(HOME)/.cargo
WASM_FLAGS := --remap-path-prefix=$(CARGO_HOME)/registry/src/=
WASM_BUILD := RUSTFLAGS='$(WASM_FLAGS)' cargo build --locked --release --target wasm32-unknown-unknown
RELEASE := wasm32-unknown-unknown/release

.PHONY: wasm test lint e2e clean

# target/wasm: the contract, its JS data driver and the test probe.
wasm:
	$(WASM_BUILD) -p dusk-multicall --features contract --target-dir target/contract
	$(WASM_BUILD) -p dusk-multicall --features data-driver-js --target-dir target/driver
	$(WASM_BUILD) -p multicall-probe --target-dir target/contract
	mkdir -p target/wasm
	cp target/contract/$(RELEASE)/dusk_multicall.wasm target/wasm/dusk_multicall.wasm
	cp target/driver/$(RELEASE)/dusk_multicall.wasm target/wasm/dusk_multicall_driver.wasm
	cp target/contract/$(RELEASE)/multicall_probe.wasm target/wasm/multicall_probe.wasm
	cd target/wasm && sha256sum *.wasm

test: wasm
	cargo test --locked -p dusk-multicall --features data-driver
	cargo test --locked -p multicall-vm-tests
	node --test tests/driver.test.mjs

lint:
	cargo fmt --all --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	cargo clippy --locked -p dusk-multicall --features data-driver --all-targets -- -D warnings
	cargo clippy --locked --target wasm32-unknown-unknown -p dusk-multicall --features contract -- -D warnings
	cargo clippy --locked --target wasm32-unknown-unknown -p multicall-probe -- -D warnings

# Starts a local Rusk 1.7.1 node, runs the end-to-end checks and stops it.
e2e:
	e2e/run.sh

clean:
	cargo clean
