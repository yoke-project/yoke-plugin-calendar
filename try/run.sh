#!/usr/bin/env bash
# Runs the application as an installed instance in a world of its own under try/.world — an install root, an
# instance's state, its secrets — grants both Plugins what their Manifests declare, and follows the unit
# calendar from the channel `panel`: every question's answer, the stream and every event, one line each. The
# world is kept between runs, so an account signed in stays signed in. An interrupt stops the instance.
#
# Usage: run.sh [--source <id>=<kind>:<location>]... [--credentials <file>] [--clients <file>] [--fresh]
#               [--online-accounts <dir>] [--seconds N]
#   --source <rule>          a source besides the demo directory, as the Plugin's argument takes it
#   --credentials <file>     the sources' credentials, JSON by source; {} where none is given
#   --clients <file>         online-accounts' clients, JSON by provider; {} where none is given
#   --online-accounts <dir>  the Plugin project online-accounts is taken from; ../../yoke-plugin-online-accounts
#   --fresh                  forgets the world first: its instance, its accounts and what was kept
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app=com.yoke.calendar-try instance=cal
online="$here/../../yoke-plugin-online-accounts"
credentials="" clients="" follow_args=() sources=()
while (( $# )); do
  case "$1" in
    --source) sources+=("$2"); shift ;;
    --credentials) credentials="$2"; shift ;;
    --clients) clients="$2"; shift ;;
    --online-accounts) online="$2"; shift ;;
    --fresh) rm -rf "$here/.world" ;;
    --seconds) follow_args+=(--seconds "$2"); shift ;;
    *) echo "run.sh: unknown argument $1" >&2; exit 2 ;;
  esac
  shift
done

# A world of its own, kept here; its sockets go under a short directory, since the longest socket path of the
# instance is computed under it and bounded.
world="$here/.world" runtime="/tmp/cal-$(id -u)"
mkdir -p "$runtime" "$world/state" "$world/config/yoke" "$world/out" "$world/apps"
chmod 2775 "$world/apps"
chmod 700 "$runtime"
export XDG_RUNTIME_DIR="$runtime" XDG_STATE_HOME="$world/state" XDG_CONFIG_HOME="$world/config"
printf 'install_root: %s\n' "$world/apps" > "$world/config/yoke/manager.conf"

# The package is made from a copy in the world, so the digests, the sources and the version written into it
# leave try/ as it is written. Both Plugins are taken again into the copy as they stand, from where they are,
# so a change to either reaches the package; the demo directory is named by its path here.
project="$world/project"
rm -rf "$project" && mkdir -p "$project"
cp -r "$here/yoke.yaml" "$here/plugins.yaml" "$here/components" "$here/manifests" "$project/"
online="$(realpath "$online")"
sed -i -e "s#^    source: \.\.\$#    source: $(realpath "$here/..")#" \
  -e "s#^    source: .*yoke-plugin-online-accounts\$#    source: $online#" "$project/plugins.yaml"
yoke-compose update plugin com.yoke.calendar --yes --dir "$project" > /dev/null
yoke-compose update plugin com.yoke.online-accounts --yes --dir "$project" > /dev/null
extra=", \"--source\", \"demo=dir:$here/demo\""
for s in "${sources[@]}"; do extra+=", \"--source\", \"$s\""; done
sed -i "s#\"--accounts\", \"\${bind.accounts}\"\]#\"--accounts\", \"\${bind.accounts}\"$extra]#" "$project/yoke.yaml"
# An installed version is never replaced: a package that changed is installed under a version of its own,
# named by what it carries, and the instance is moved to it.
written="$(sed -n 's/^version: //p' "$project/yoke.yaml")"
version="$written-try-$(cat "$project/yoke.yaml" "$project/manifests/"* | sha256sum | cut -c1-12)"
yoke-compose set version "$version" --dir "$project" > /dev/null
cargo build --release --quiet --manifest-path "$here/follow/Cargo.toml"
rm -f "$world/out/"*.yoke
yoke-compose build --dir "$project" --out "$world/out" > /dev/null
yoke instance stop "$instance" > /dev/null 2>&1 || true
yoke app install "$world/out/"*.yoke > /dev/null
if shown="$(yoke instance show "$instance" 2>/dev/null)"; then
  was="$(sed -n 's/^version: //p' <<<"$shown")"
  if [[ "$was" != "$version" ]]; then
    yoke instance set-version "$instance" "$version" > /dev/null
    yoke app remove "$app" --version "$was" > /dev/null
  fi
else
  yoke instance create "$app" --name "$instance" --version "$version" > /dev/null
  echo '{}' | yoke instance set-secret "$instance" tokens > /dev/null
  echo '{}' | yoke instance set-secret "$instance" clients > /dev/null
  echo '{}' | yoke instance set-secret "$instance" credentials > /dev/null
fi
[[ -z "$clients" ]] || yoke instance set-secret "$instance" clients < "$clients" > /dev/null
[[ -z "$credentials" ]] || yoke instance set-secret "$instance" credentials < "$credentials" > /dev/null

yoke instance start "$instance" > /dev/null
trap 'yoke instance stop "$instance" > /dev/null 2>&1' EXIT
until yoke core status --instance "$instance" > /dev/null 2>&1; do sleep 0.3; done
# An instance starts with nothing granted; a grant reaches the unit at its next admission.
for manifest in "$here/manifests/"*.yaml; do
  plugin="$(sed -n 's/^id: //p' "$manifest")"
  for capability in $(sed -n 's/^  - name: //p' "$manifest"); do
    yoke plugin grant "$plugin" "$capability" --instance "$instance" > /dev/null
  done
done
yoke unit restart online-accounts --instance "$instance" > /dev/null
yoke unit restart calendar --instance "$instance" > /dev/null
sleep 2
echo "run.sh: $instance runs; with: XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR XDG_STATE_HOME=$XDG_STATE_HOME XDG_CONFIG_HOME=$XDG_CONFIG_HOME yoke unit ask calendar calendar.days --instance $instance" >&2
"$here/follow/target/release/follow" --instance "$instance" --channel panel --unit calendar \
  --stream calendar.upcoming --ask calendar.sources "${follow_args[@]}"
