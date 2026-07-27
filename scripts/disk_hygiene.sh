#!/usr/bin/env bash
set -euo pipefail

# Inventory only: this command never deletes or mutates artifacts.
root=${1:-/data/tmp/fgdb_swarm}
classify() {
  case "$1" in
    *xfrc*) printf 'pane7\tfgdb-xfrc\tCLOSED\t[B]';;
    *2pbf*) printf 'pane5\tfgdb-2pbf\tCLOSED\t[B]';;
    *kwoz*) printf 'pane6\tfgdb-kwoz\tIN_PROGRESS\t[R]~7s';;
    *uug4*) printf 'pane3\tfgdb-uug4\tCLOSED\t[B]';;
    *njpk*) printf 'pane5\tfgdb-njpk\tCLOSED\t[B]';;
    *ymqm*) printf 'pane7\tfgdb-ymqm\tCLOSED\t[B]';;
    *dkjg*) printf 'pane5\tfgdb-dkjg\tIN_PROGRESS\t[R]~7s';;
    *) printf 'unknown\tunknown\tUNKNOWN\t[!]';;
  esac
}
printf 'KB\tPATH\tOWNER\tBEAD\tSTATUS\tDISPOSITION\n'
while IFS=$'\t' read -r kb path; do
  printf '%s\t%s\t%s\n' "$kb" "$path" "$(classify "$path")"
done < <(du -sk "$root"/* 2>/dev/null | awk '$1 >= 51200' | sort -nr)
