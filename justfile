# Logigator-sim task runner. Covers the core + CLI + WASM + Node surfaces and the corpus tooling.

# List the available recipes.
default:
    @just --list

# Build everything (the CLI + the default WASM package + the Node addon).
build: build-cli build-wasm build-node

# Release CLI binary → target/release/sim.
build-cli:
    cargo build -p sim-cli --release

# Default WASM package → crates/sim-wasm/pkg (single-threaded, SIMD128, web target).
build-wasm:
    RUSTFLAGS="-C target-feature=+simd128" \
        wasm-pack build crates/sim-wasm --release --target web -- --no-default-features --features serde

# Node-target WASM package → crates/sim-wasm/pkg-bench-node (what `bench-wasm` loads).
build-wasm-node:
    RUSTFLAGS="-C target-feature=+simd128" \
        wasm-pack build crates/sim-wasm --release --target nodejs --out-dir pkg-bench-node -- --no-default-features --features serde

# Install the Node addon's npm deps (@napi-rs/cli); run once before build-node / test-node.
setup-node:
    cd crates/sim-node && npm install

# Debug Node addon → crates/sim-node/{index.js,index.d.ts,*.node}; enough for the test suite.
build-node:
    cd crates/sim-node && npx napi build --platform

# Release Node addon — required for `bench-node` (a debug addon measures the wrong thing).
build-node-release:
    cd crates/sim-node && npx napi build --platform --release

# Format check + lint + the full host test suite (includes the tick-exact golden corpus).
test:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Cross-engine equivalence: the corpus run through the WASM binding under Node.
test-wasm:
    wasm-pack test --node crates/sim-wasm

# Cross-engine equivalence + async surface: the corpus run through the Node binding.
test-node: build-node
    cd crates/sim-node && node --test __test__/*.mjs

# Apply rustfmt across the workspace.
fmt:
    cargo fmt --all

# Throughput smoke for one board (extra args pass through, e.g. --ticks N --repeat R).
bench board="corpus/boards/clk.json" *args="": build-cli
    ./target/release/sim bench {{board}} {{args}}

# Dump a board's per-tick trace in golden format to stdout (usage: just trace corpus/boards/gates.json).
trace board *args="": build-cli
    ./target/release/sim trace {{board}} {{args}}

# Install the corpus tools' npm deps (the C++ oracle); run once before gen-golden / bench-cpp.
setup-corpus:
    cd corpus/tools && npm install

# Regenerate every per-tick golden trace from the published C++ oracle → corpus/golden/.
gen-golden:
    cd corpus/tools && npm run gen

# Regenerate the synthetic benchmark boards → corpus/bench/ (deterministic, byte-for-byte).
gen-bench:
    cd corpus/tools && node gen-bench.mjs

# Bench one board through the Node binding (extra args: --ticks N --repeat R).
bench-node board *args="": build-node-release
    node corpus/tools/bench-node.mjs {{board}} {{args}}

# Bench one board through the WASM binding under Node (extra args: --ticks N --repeat R).
bench-wasm board *args="": build-wasm-node
    node corpus/tools/bench-wasm.mjs {{board}} {{args}}

# Bench one board through the old C++ engine — the rewrite's reference point (needs setup-corpus).
bench-cpp board *args="":
    node corpus/tools/bench-cpp-node.mjs {{board}} {{args}}
