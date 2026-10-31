#!/bin/sh
# Synthetic Cargo: reject the wrong command, then simulate its exit status.
[ "$#" -eq 1 ] && [ "$1" = test ] || exit 99
exit "$XTASK_CARGO_EXIT"
