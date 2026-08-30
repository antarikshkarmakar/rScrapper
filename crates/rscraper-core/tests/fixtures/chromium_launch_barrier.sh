#!/bin/sh
set -eu

stderr_file="$RSCRAPER_LAUNCH_BARRIER_DIR/chromium.stderr"
pid_file="$RSCRAPER_LAUNCH_BARRIER_DIR/chromium.pid"
reached_file="$RSCRAPER_LAUNCH_BARRIER_DIR/reached"
release_file="$RSCRAPER_LAUNCH_BARRIER_DIR/release"
stderr_fifo="$RSCRAPER_LAUNCH_BARRIER_DIR/chromium.stderr.fifo"
filter_pid_file="$RSCRAPER_LAUNCH_BARRIER_DIR/filter.pid"
owner_pid=$$

mkfifo "$stderr_fifo"
printf '%s\n' "$owner_pid" >"$pid_file"

(
    while IFS= read -r line; do
        printf '%s\n' "$line" >>"$stderr_file"
        case "$line" in
            "DevTools listening on "*)
                printf '%s\n' reached >"$reached_file"
                while [ ! -e "$release_file" ]; do
                    if ! kill -0 "$owner_pid" 2>/dev/null; then
                        exit 0
                    fi
                    sleep 0.01
                done
                printf '%s\n' "$line" >&2
                ;;
        esac
    done <"$stderr_fifo"
) &
printf '%s\n' "$!" >"$filter_pid_file"

exec "$RSCRAPER_REAL_CHROMIUM" "$@" 2>"$stderr_fifo"
