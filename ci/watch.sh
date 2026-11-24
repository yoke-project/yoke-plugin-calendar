#!/usr/bin/env bash
# The watch: this Plugin against what Yoke last published. The newest programs are installed from the module
# proxy; the Plugin and online-accounts, which its need accounts is bound to, are built and taken into try/ by
# them as anyone's would be, packaged, installed as an instance with every secret empty and one source, a
# directory of one event, run under a real Core, and asked its sources. The answer names the source read:
# the Plugin admitted with its need bound, reading, and answering under the release.
# Usage: watch.sh [results directory]
#   YOKE_PROGRAMS=<dir> runs it with the programs there instead of the newest release's, for a build of Yoke
#   not published yet.
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
results="${1:-$root/.results}"
mkdir -p "$results" && results="$(cd "$results" && pwd)" # absolute: the script moves into try/
date -u +%Y-%m-%dT%H:%M:%SZ > "$results/started"
CASE=yoke-plugin-calendar:the-watch.01

finish() { # <verdict line>
  echo "$1" | tee "$results/checks.txt"
  date -u +%Y-%m-%dT%H:%M:%SZ > "$results/finished"
  [[ "$1" == pass* ]]
  exit $?
}

# Short, since the longest socket path of the instance is computed under it and bounded.
work="$(mktemp -d /tmp/w.XXX)"
trap 'yoke instance stop watch > /dev/null 2>&1; rm -rf "$work"' EXIT
mkdir -p "$work/bin" "$work/r" "$work/state" "$work/config/yoke" "$work/out" "$work/apps" "$work/demo"
chmod 2775 "$work/apps"
chmod 700 "$work/r"
export GOBIN="$work/bin" PATH="$work/bin:$PATH" XDG_RUNTIME_DIR="$work/r" XDG_STATE_HOME="$work/state" XDG_CONFIG_HOME="$work/config"
printf 'install_root: %s\n' "$work/apps" > "$work/config/yoke/manager.conf"

if [[ -n "${YOKE_PROGRAMS:-}" ]]; then
  release="the programs in $YOKE_PROGRAMS"
  export PATH="$YOKE_PROGRAMS:$PATH"
else
  release="$(go list -m -f '{{.Version}}' github.com/yoke-project/yoke@latest)" || finish "FAIL  $CASE — the newest release of yoke cannot be read"
  for program in yoke yoke-compose yoke-core yoke-guardian yoke-auth; do
    go install "github.com/yoke-project/yoke/cmd/$program@$release" || finish "FAIL  $CASE — $program $release does not install"
  done
fi
echo "watch: yoke $release"
echo "$release" > "$results/release"

# online-accounts where try/plugins.yaml takes it from: beside this repository.
online="$root/../yoke-plugin-online-accounts"
[[ -d "$online" ]] || git clone -q https://github.com/yoke-project/yoke-plugin-online-accounts "$online" || finish "FAIL  $CASE — online-accounts cannot be cloned"

cd "$root/try" || finish "FAIL  $CASE — no try/"
rm -f components/plugins/* manifests/*.yaml
mkdir -p components/plugins
for plugin in com.yoke.online-accounts com.yoke.calendar; do
  said="$(yoke-compose update plugin "$plugin" --yes 2>&1)" || finish "FAIL  $CASE — $plugin is not taken under $release: $(tail -n 2 <<<"$said" | tr '\n' ' ')"
done
cp demo/demo.ics "$work/demo/"
sed -i "s#\"--accounts\", \"\${bind.accounts}\"\]#\"--accounts\", \"\${bind.accounts}\", \"--source\", \"demo=dir:$work/demo\"]#" yoke.yaml
said="$(yoke-compose build --out "$work/out" 2>&1)" || finish "FAIL  $CASE — the application is not packaged under $release: $(tail -n 2 <<<"$said" | tr '\n' ' ')"
said="$(yoke app install "$work/out/"*.yoke 2>&1)" || finish "FAIL  $CASE — the package is not installed under $release: $(tail -n 2 <<<"$said" | tr '\n' ' ')"
yoke instance create com.yoke.calendar-try --name watch > /dev/null || finish "FAIL  $CASE — no instance is created under $release"
for secret in credentials clients tokens; do
  echo '{}' | yoke instance set-secret watch "$secret" > /dev/null || finish "FAIL  $CASE — the secret $secret is not set"
done
said="$(yoke instance start watch 2>&1)" || finish "FAIL  $CASE — the instance did not start under $release: $(tail -n 3 <<<"$said" | tr '\n' ' ')"
for _ in $(seq 1 30); do
  yoke core status --instance watch > /dev/null 2>&1 && break
  sleep 1
done

# std: yoke-plugin-calendar:the-watch.01
probe() { # says why on a failure
  yoke plugin grant com.yoke.calendar query.calendar.sources.answer --instance watch > /dev/null || { echo "the grant was refused"; return 1; }
  yoke unit restart calendar --instance watch > /dev/null || { echo "the unit did not restart"; return 1; }
  local said
  for _ in $(seq 1 10); do
    sleep 1
    said="$(yoke unit ask calendar calendar.sources --instance watch 2>&1)" || continue
    grep -q '"state":"ok"' <<<"$said" && return 0
  done
  echo "the sources are not answered with the demo read: $said"
  return 1
}

if probe > "$work/why"; then verdict="pass  $CASE"; else verdict="FAIL  $CASE — under $release: $(cat "$work/why")"; fi
git -C "$root" checkout -q -- try/yoke.yaml try/plugins.yaml try/manifests 2>/dev/null
finish "$verdict"
