#!/usr/bin/env bash
# Sourced by setup-env.sh, start-guest.sh, and diagnose.sh -- the fixed
# mount point every WM container uses for the harness's own files
# (scripts + container defs), used to be defined independently in all
# three (confirmed via an audit while planning Phase 8's packaging
# work: three separate literal "/workspace" definitions, not one shared
# fact). Not a user-facing name -- it doesn't matter what's mounted
# there or where it came from, only that every script and Containerfile
# agrees on where to find it inside a container.

# Used by whichever script sources this file, not here -- shellcheck
# can't see across the source boundary.
# shellcheck disable=SC2034
HARNESS_CONTAINER_ROOT="/workspace"
