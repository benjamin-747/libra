#!/bin/sh
# Fake investigator: CONCLUDES and additionally emits a runtime-assembled
# fake `sk-` credential plus an ANSI escape sequence. The workflow test
# asserts the concluding stance is persisted, but the fake secret never
# survives the redaction pipeline into findings.md or the *.redacted.log
# files, and the ANSI escape is scrubbed from the persisted logs.
#
# The credential is assembled at run time from an `sk-%s` format string
# so no token-shaped literal lives in the repository (agent.md fixture
# rule); POSIX sh builtins only, env_clear + empty environment.
printf '%s\n' 'conclude: leak confirmed in cache.rs'
printf -- '- fake credential for redaction proof: sk-%s\n' 'abcdefghijklmnopqrstuvwx123456'
printf -- '- ansi smuggle attempt: \033[31mnot-really-red\033[0m\n'
exit 0
