#!/usr/bin/env bash
# Deploy reviewed local commits without publishing them to a Git hosting service.
# Usage: bin/deploy-server.sh SSH_HOST [--restart]
# The remote checkout must be clean and an ancestor of local HEAD.
#
# The web remote is served from the server's checkout, so it goes live the moment the
# checkout moves, while the running binary stays the old one until a restart. A deploy
# whose remote calls an endpoint the old binary lacks therefore needs --restart in the
# same run (or a second run with --restart: with nothing left to send, this script
# only restarts).
set -euo pipefail
host=${1:?Usage: deploy-server.sh SSH_HOST [--restart]}
mode=${2:---stage}
[[ "$host" != -* && "$host" =~ ^[a-zA-Z0-9_.@-]+$ ]]
[[ "$mode" == --stage || "$mode" == --restart ]]
cd "$(dirname "$0")/.."
test -z "$(git status --porcelain)" || { echo 'Commit reviewed changes before deploying.' >&2; exit 1; }
base=$(ssh "$host" 'cd ~/Projects/spela && test -z "$(git status --porcelain)" && git rev-parse HEAD')
git merge-base --is-ancestor "$base" HEAD
# "-" means the server already has every commit (an earlier staged deploy): git
# refuses to create an empty bundle, and there is nothing to send anyway.
remote_bundle=-
if [[ "$base" != "$(git rev-parse HEAD)" ]]; then
    bundle=$(mktemp -t spela-deploy.XXXXXX)
    trap 'rm -f "$bundle"' EXIT
    git bundle create "$bundle" "$base..HEAD"
    remote_bundle=$(ssh "$host" 'mktemp /tmp/spela-deploy.XXXXXX')
    scp -q "$bundle" "$host:$remote_bundle"
fi
ssh "$host" bash -s -- "$remote_bundle" "$base" "$mode" <<'REMOTE'
set -euo pipefail
bundle=$1
base=$2
mode=$3
cd ~/Projects/spela
test -z "$(git status --porcelain)"
test "$(git rev-parse HEAD)" = "$base"
if [[ "$bundle" != - ]]; then
    trap 'rm -f "$bundle"' EXIT
    git fetch "$bundle" HEAD
    git merge --ff-only FETCH_HEAD
fi
# A no-op when the binary is already built from this checkout.
nice -n19 cargo build --release --locked
if [[ "$mode" == --restart ]]; then
    # Never interrupt a live stream or a pending play. A wedged HTTP endpoint
    # needs operator diagnosis; --restart is for an already-confirmed idle host.
    curl --fail --silent --max-time 5 http://127.0.0.1:7890/status |
        python3 -c 'import json,sys; s=json.load(sys.stdin); sys.exit(0 if s.get("status")=="idle" else "Playback active; binary staged only")'
    curl --fail --silent --max-time 5 http://127.0.0.1:7890/progress |
        python3 -c 'import json,sys; s=json.load(sys.stdin); sys.exit(1 if s.get("active") else 0)'
    sudo systemctl restart spela
    curl --retry 10 --retry-connrefused --retry-delay 1 --fail --silent --max-time 10 http://127.0.0.1:7890/status
fi
REMOTE
