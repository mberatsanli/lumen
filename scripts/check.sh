#!/usr/bin/env bash
set -euo pipefail

# Hard cap: no verification step may exceed 10 minutes. Uses perl's alarm
# (macOS has no `timeout`); a stuck compile/test dies instead of hanging.
limit() {
  perl -e 'alarm shift; exec @ARGV or die "exec: $!"' 600 "$@"
}

limit cargo fmt --all -- --check
limit cargo clippy --workspace --all-targets --all-features -- -D warnings
limit cargo test --workspace
