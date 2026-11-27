#!/bin/bash
# Prints the [workspace.package] version of Cargo.toml (or of the file given as the first argument).
set -euo pipefail
file=${1:-"$(dirname "$0")/../Cargo.toml"}
version=$(awk '
  /^\[/ { in_pkg = ($0 == "[workspace.package]") }
  in_pkg && /^version[[:space:]]*=/ { gsub(/^[^"]*"|".*$/, ""); print; exit }
' "$file")
if [[ -z "$version" ]]; then
  echo "version.sh: no version in [workspace.package] of $file" >&2
  exit 1
fi
echo "$version"
