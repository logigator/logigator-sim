# Logigator-sim task runner (plan §4.6). The Node build recipe lands with its crate in plan
# phase 5; this is the phase-4 (core + CLI + WASM) subset.

# List the available recipes.
default:
    @just --list

# Build everything that exists today (the CLI + the default WASM package).
build: build-cli build-wasm

# Release CLI binary → target/release/sim.
build-cli:
    cargo build -p sim-cli --release

# Default WASM package → crates/sim-wasm/pkg (single-threaded, SIMD128, web target; plan §4.2).
build-wasm:
    RUSTFLAGS="-C target-feature=+simd128" \
        wasm-pack build crates/sim-wasm --release --target web -- --no-default-features --features serde

# Format check + lint + the full host test suite (includes the tick-exact golden corpus, §10.1).
test:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Cross-engine equivalence: the corpus run through the WASM binding under Node (plan §10.1, phase 4).
test-wasm:
    wasm-pack test --node crates/sim-wasm

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
