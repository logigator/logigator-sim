# Logigator-sim task runner (plan §4.6). The WASM / Node build recipes land with their crates
# in plan phases 4–5; this is the phase-3 (core + CLI) subset.

# List the available recipes.
default:
    @just --list

# Build everything that exists today (the CLI; sim-core is a library dependency of it).
build: build-cli

# Release CLI binary → target/release/sim.
build-cli:
    cargo build -p sim-cli --release

# Format check + lint + the full test suite (includes the tick-exact golden-trace corpus, §10.1).
test:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

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
