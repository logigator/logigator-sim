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

# Stamps CHANGELOG.md's [Unreleased] notes as the new version (refuses while they are empty)
# and updates Cargo.toml, package.json, and the lockfiles; commit, then tag vX.Y.Z.
# Bump the release version everywhere it lives.
bump version:
    node scripts/bump-version.mjs {{version}}

# Edit the output into CHANGELOG.md's [Unreleased] section. Extra args pass to git-cliff.
# Draft changelog entries from the conventional commits since the last tag (see cliff.toml).
changelog *args="":
    npx --yes git-cliff@2.14.2 --unreleased --strip all {{args}}

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

# Throughput smoke for one board (time-bound by default; extra args pass through, e.g. --ms N --repeat R).
bench board="corpus/boards/clk.json" *args="": build-cli
    ./target/release/sim bench {{board}} {{args}}

# Dump a board's per-tick trace in golden format to stdout (usage: just trace corpus/boards/gates.json).
trace board *args="": build-cli
    ./target/release/sim trace {{board}} {{args}}

# Regenerate every per-tick golden trace from the Rust CLI → corpus/golden/.
gen-golden: build-cli
    cd corpus/tools && npm run gen

# Regenerate the synthetic benchmark boards → corpus/bench/ (deterministic, byte-for-byte).
gen-bench:
    cd corpus/tools && npm run gen-bench

# Bench one board through the Node binding (time-bound by default; extra args: --ms N --repeat R).
bench-node board *args="": build-node-release
    cd corpus/tools && npm run bench-node -- {{board}} {{args}}

# Bench one board through the WASM binding's blocking run() under Node (extra args: --ms N --repeat R).
bench-wasm board *args="": build-wasm
    cd corpus/tools && npm run bench-wasm -- {{board}} {{args}}

# Bench one board through the WASM binding's async runAsync() under Node (extra args: --ms N --repeat R).
bench-wasm-async board *args="": build-wasm
    cd corpus/tools && npm run bench-wasm-async -- {{board}} {{args}}

# Interleaved before/after bench for one surface (cli|node|wasm|wasm-async) against a git ref.
# Builds the ref in a throwaway worktree + the working tree, runs each interleaved per board.
# Usage: just compare cli --before main   (extra args: --ms N --repeat R --boards a,b --cores LIST --keep).
compare surface *args="":
    node corpus/tools/bench-compare.mjs {{surface}} {{args}}
