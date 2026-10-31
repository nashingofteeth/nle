#!/usr/bin/env bash
# Install the `nile` binary and create the compatibility `nle` alias.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo install --locked --force --path crates/nle-cli
bin_dir="${CARGO_HOME:-$HOME/.cargo}/bin"
ln -sf "$bin_dir/nile" "$bin_dir/nle"
