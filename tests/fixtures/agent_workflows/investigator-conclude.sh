#!/bin/sh
# Fake investigator: reports a CONCLUDING stance. The output carries the
# token 'conclude', so `classify_stance_disposition` marks the turn
# `concluding` and it counts toward quorum. Exit 0.
#
# POSIX sh builtins only; runs under env_clear with an empty environment.
printf '%s\n' 'STANCE: concluding'
printf '%s\n' 'conclude: the root cause is the cold-start cache rebuild in cache.rs:42'
exit 0
