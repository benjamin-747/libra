#!/bin/sh
# Fake investigator: reports a NON-concluding stance (no 'conclud' token,
# so it stays `continuing` and never counts toward quorum) with a small,
# known finding. Exit 0. Drives the max-turns and round-robin-order
# scenarios where the investigation never converges.
#
# POSIX sh builtins only; runs under env_clear with an empty environment.
printf '%s\n' 'still investigating; the hot path needs another pass'
exit 0
