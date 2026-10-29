#!/usr/bin/env bash
# The verify-before-every-commit sequence (see AGENTS.md "Verify before every
# commit"). Run from anywhere: it enters the repo root first.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check                      # formatting is enforced
if command -v shellcheck >/dev/null; then    # CI always has it
  shellcheck scripts/check.sh demo.sh
else
  echo "shellcheck not installed — skipping (CI runs it)" >&2
fi
cargo clippy --locked --all-targets -- -D warnings   # warnings are errors
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps  # broken doc links fail
cargo test --locked                          # unit + behaviour + hostile batteries
cargo build --locked --release
cargo install --locked --path crates/nle-cli
./demo.sh                                    # end-to-end pixel checks (release binary)
