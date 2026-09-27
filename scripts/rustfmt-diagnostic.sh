#!/usr/bin/env bash
set -uo pipefail

rustfmt "$@"
format_status=$?

if mkdir /tmp/aegis-clippy-diagnostic-lock 2>/dev/null; then
  (
    unset RUSTFMT
    cargo clippy --all-targets -- -D warnings
  ) > .ci-clippy.log 2>&1 || true
fi

exit "$format_status"
