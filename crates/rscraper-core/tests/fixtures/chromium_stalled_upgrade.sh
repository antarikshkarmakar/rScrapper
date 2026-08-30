#!/bin/sh
set -eu

pid_file="$RSCRAPER_STALLED_UPGRADE_DIR/child.pid"
reached_file="$RSCRAPER_STALLED_UPGRADE_DIR/reached"
proxy_file="$RSCRAPER_STALLED_UPGRADE_DIR/proxy.address"

for argument in "$@"; do
    case "$argument" in
        --proxy-server=http://*)
            printf '%s\n' "${argument#--proxy-server=http://}" >"$proxy_file"
            ;;
    esac
done

printf '%s\n' "$$" >"$pid_file"
printf '%s\n' "DevTools listening on $RSCRAPER_STALLED_DEVTOOLS_URL" >&2
printf '%s\n' reached >"$reached_file"

exec sleep 3600
