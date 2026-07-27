#!/usr/bin/env bash
set -euo pipefail

# Inventory only: this command never deletes or mutates artifacts.
root=${1:-/data/tmp/fgdb_swarm}
du -sk "$root"/* 2>/dev/null | awk '$1 >= 51200 { print $1 " KB\t" $2 }' | sort -nr
