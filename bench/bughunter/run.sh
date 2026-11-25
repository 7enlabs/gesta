#!/usr/bin/env bash
# Run only local, disposable fixtures against the supplied checkout.
set -euo pipefail
if [[ $# -ne 1 ]]; then
    printf 'usage: bash run.sh <gesta-checkout>\n' >&2
    exit 2
fi
script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
exec python3 "$script_dir/scenarios.py" "$1"
