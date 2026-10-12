#!/usr/bin/env bash
# The verify-before-every-commit sequence (see AGENTS.md "Verify before every
# commit"). Run from anywhere: it enters the repo root first.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check                      # formatting is enforced
cargo clippy --all-targets -- -D warnings    # warnings are errors
cargo test                                   # full workspace, must be green
cargo build --release
cargo install --path crates/nle-cli
./demo.sh                                    # end-to-end pixel checks
./scripts/expect.sh                          # expected behaviours, asserted
./scripts/fuzz.sh                            # hostile input: no panic, no corruption
