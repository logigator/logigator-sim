# Logigator-sim task runner (plan §4.6). Covers the core + CLI + WASM + Node surfaces; the
# threaded-WASM variant and the full Node prebuild matrix land in later phases.

# List the available recipes.
default:
    @just --list

# Build everything that exists today (the CLI + the default WASM package + the Node addon).
build: build-cli build-wasm build-node

# Release CLI binary → target/release/sim.
build-cli:
    cargo build -p sim-cli --release

# Default WASM package → crates/sim-wasm/pkg (single-threaded, SIMD128, web target; plan §4.2).
build-wasm:
    RUSTFLAGS="-C target-feature=+simd128" \
        wasm-pack build crates/sim-wasm --release --target web -- --no-default-features --features serde

# Format check + lint + the full host test suite (includes the tick-exact golden corpus, §10.1).
# The default `cargo test --workspace` exercises the single-threaded engine; the extra sim-core
# run with `--features threads` adds the adaptive parallel driver: clippy on its `PAR=true` code,
# the ST≡MT property + corpus-equivalence + JK-race tests (plan §8.6/§10.1, phase 6).
test:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy -p sim-core --features threads --all-targets -- -D warnings
    cargo test --workspace
    cargo test -p sim-core --features threads

# Cross-engine equivalence: the corpus run through the WASM binding under Node (plan §10.1, phase 4).
test-wasm:
    wasm-pack test --node crates/sim-wasm

# Install the Node addon's npm deps (@napi-rs/cli); run once before build-node / test-node.
setup-node:
    cd crates/sim-node && npm install

# Build the Node addon (debug) → crates/sim-node/{index.js,index.d.ts,*.node}; `--release` is for
# the published prebuilds — debug is enough for the test suite (plan §4.3).
build-node:
    cd crates/sim-node && npx napi build --platform

# Cross-engine equivalence + async surface: the corpus run through the Node binding (plan §10.1).
test-node: build-node
    cd crates/sim-node && node --test __test__/*.mjs

# Apply rustfmt across the workspace.
fmt:
    cargo fmt --all

# Verify the ported fixtures against their expected final state (exit 1 on mismatch).
verify: build-cli
    ./target/release/sim verify corpus/fixtures/*.json

# Throughput smoke for one board (default: the clock).
bench board="corpus/boards/clk.json": build-cli
    ./target/release/sim bench {{board}}

# Dump a board's per-tick trace in golden format to stdout (usage: just trace corpus/boards/gates.json).
trace board: build-cli
    ./target/release/sim trace {{board}}
