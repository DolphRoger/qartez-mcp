#!/usr/bin/env bash
# ci-retry.sh - Retry a cargo command on transient network failures.
#
# The ort-sys crate's build script downloads prebuilt ONNX Runtime
# binaries from cdn.pyke.io at `--all-features` build time. A 5xx from
# that CDN (seen on v0.9.4's ubuntu-stable job: HTTP 504) is unrelated
# to the code under test and should not red-tag a release. This wrapper
# retries the command up to 3 times with backoff, but ONLY when the
# stderr captures a transient-network signature. Deterministic failures
# (clippy lints, test assertions, compile errors) surface immediately.

set -o pipefail

attempts=3
attempt=0
while :; do
    attempt=$((attempt + 1))
    log=$(mktemp)
    if "$@" 2>&1 | tee "$log"; then
        rm -f "$log"
        exit 0
    fi
    if ! grep -qE "http status: 5[0-9][0-9]|failed to download|Connection (reset|refused|timed out)|Operation timed out|spurious network error|Temporary failure in name resolution|Could not resolve host|DNS (resolution|lookup) failed" "$log"; then
        rm -f "$log"
        echo "::error::non-retryable failure"
        exit 1
    fi
    rm -f "$log"
    if [ "$attempt" -ge "$attempts" ]; then
        echo "::error::exhausted ${attempts} retries on transient network failure"
        exit 1
    fi
    backoff=$((attempt * 15))
    echo "::warning::transient network failure detected, retrying in ${backoff}s (attempt $((attempt + 1))/${attempts})..."
    sleep "$backoff"
done
