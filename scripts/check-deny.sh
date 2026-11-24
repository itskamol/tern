#!/usr/bin/env bash
# Runs cargo-deny without the ~500 KB duplicate-crate warning trees so real failures stay legible.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo deny check -A duplicate -A license-not-encountered -s "$@"
