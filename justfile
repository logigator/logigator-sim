# Logigator-sim task runner. Covers the core + CLI + WASM + Node surfaces and the corpus tooling.

# List the available recipes.
default:
    @just --list

# Build everything (the CLI + the default WASM package + the Node addon).
build: build-cli build-wasm build-node

# Release CLI binary → target/release/sim.
build-cli:
    cargo build -p sim-cli --release

# Default WASM package → crates/sim-wasm/pkg (single-threaded, SIMD128, web target,
# published as @logigator/sim-wasm).
build-wasm:
    RUSTFLAGS="-C target-feature=+simd128" \
        wasm-pack build crates/sim-wasm --release --target web --scope logigator -- --no-default-features --features serde

# Install the Node addon's npm deps (@napi-rs/cli); run once before build-node / test-node.
setup-node:
    cd crates/sim-node && npm install

# Debug Node addon → crates/sim-node/{index.js,index.d.ts,*.node}; enough for the test suite.
build-node:
    cd crates/sim-node && npx napi build --platform --esm

# Release Node addon — required for `bench-node` (a debug addon measures the wrong thing).
# `release-addon` is the release profile with unwinding, so panics become JS exceptions.
build-node-release:
    cd crates/sim-node && npx napi build --platform --esm --profile release-addon

# Bump the release version everywhere it lives (workspace Cargo.toml, package.json,
# lockfiles); commit the result before tagging vX.Y.Z.
bump version:
    node scripts/bump-version.mjs {{version}}

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

# Install the corpus tools' npm deps (the C++ oracle); run once before bench-cpp.
setup-corpus:
    cd corpus/tools && npm install

# Regenerate every per-tick golden trace from the Rust CLI → corpus/golden/.
gen-golden: build-cli
    cd corpus/tools && npm run gen

# Regenerate the synthetic benchmark boards → corpus/bench/ (deterministic, byte-for-byte).
gen-bench:
    cd corpus/tools && npm run gen-bench

# Bench one board through the Node binding (extra args: --ticks N --repeat R).
bench-node board *args="": build-node-release
    cd corpus/tools && npm run bench-node -- {{board}} {{args}}

# Bench one board through the WASM binding under Node (extra args: --ticks N --repeat R).
bench-wasm board *args="": build-wasm
    cd corpus/tools && npm run bench-wasm -- {{board}} {{args}}

# Bench one board through the old C++ engine — the rewrite's reference point (needs setup-corpus).
bench-cpp board *args="":
    cd corpus/tools && npm run bench-cpp -- {{board}} {{args}}
