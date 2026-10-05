#!/usr/bin/env bash
#
# |cargo install| of the top-level crate will not install binaries for
# other workspace crates or native program crates.
#
# Implemented by `cargo xtask install-all`; run with --help for options.
# Kept as a wrapper for existing callers.

set -e

root="$(cd "$(dirname "$0")/.." && pwd)"

args=()
for arg in "$@"; do
  if [[ ${arg:0:1} = + ]]; then
    args+=(--toolchain "${arg:1}")
  else
    args+=("$arg")
  fi
done

exec "$root"/cargo run --quiet --manifest-path "$root"/ci/xtask/Cargo.toml --bin xtask -- install-all "${args[@]}"
