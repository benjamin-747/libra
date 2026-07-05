#!/bin/sh
# Fake investigator: exits 0 with NO stdout — a successful turn that
# produced no new findings. The engine classifies this as a *stall* and
# PAUSES the run (PauseReason::Stalled) rather than recording a stance,
# so `investigate continue` can resume it.
#
# POSIX sh builtins only; runs under env_clear with an empty environment.
exit 0
