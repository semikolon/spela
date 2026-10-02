#!/usr/bin/env bash
# Deploy reviewed local commits without publishing them to a Git hosting service.
# Usage: bin/deploy-server.sh SSH_HOST [--restart]
# The remote checkout must be clean and an ancestor of local HEAD.
set -euo pipefail
host=${1:?Usage: deploy-server.sh SSH_HOST [--restart]}
mode=${2:---stage}
[[ "$host" != -* && "$host" =~ ^[a-zA-Z0-9_.@-]+$ ]]
[[ "$mode" == --stage || "$mode" == --restart ]]
cd "$(dirname "$0")/.."
test -z "$(git status --porcelain)" || { echo 'Commit reviewed changes before deploying.' >&2; exit 1; }
base=$(ssh "$host" 'cd ~/Projects/spela && test -z "$(git status --porcelain)" && git rev-parse HEAD')
git merge-base --is-ancestor "$base" HEAD
bundle=$(mktemp -t spela-deploy.XXXXXX)
trap 'rm -f "$bundle"' EXIT
git bundle create "$bundle" "$base..HEAD"
remote_bundle=$(ssh "$host" 'mktemp /tmp/spela-deploy.XXXXXX')
scp -q "$bundle" "$host:$remote_bundle"
ssh "$host" bash -s -- "$remote_bundle" "$base" "$mode" <<'REMOTE'
set -euo pipefail
bundle=$1
base=$2
mode=$3
trap 'rm -f "$bundle"' EXIT
cd ~/Projects/spela
test -z "$(git status --porcelain)"
test "$(git rev-parse HEAD)" = "$base"
git fetch "$bundle" HEAD
git merge --ff-only FETCH_HEAD
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
