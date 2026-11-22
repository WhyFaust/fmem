#!/bin/sh
binary="${PLUGIN_ROOT:-}/fmem"
if [ -n "${PLUGIN_ROOT:-}" ] && [ -x "$binary" ]; then
    if [ "$1" = 'WorkerHook' ]; then
        exec "$binary" codex worker-hook
    fi
    exec "$binary" codex hook "$1"
fi
printf '%s\n' '{}'
