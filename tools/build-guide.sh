#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cc -O2 -Wall -Wextra -Werror \
  "$script_dir/pikscreen-guide.c" \
  -o "$script_dir/pikscreen-guide" \
  $(pkg-config --cflags --libs gtk4 gtk4-layer-shell-0) \
  -lm
