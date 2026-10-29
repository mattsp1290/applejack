#!/bin/sh
# Stand-in for `applejack serve --stdio` used by CoreClientTests.
#
# Answers `hello` (protocol 1 only; anything else gets ProtocolMismatch and
# exit 65), `host.doctor` (canned facts), and `fake.echo` (returns its params;
# a `delay_ms` member delays that one answer in the background so a later
# request is answered first). Every other method gets NotImplemented. A line
# that does not look like what `CoreClient` sends (sorted keys, numeric id)
# gets InvalidRequest.
#
#   FAKE_CORE_SPLIT=1          write each response in two chunks with a pause
#   FAKE_CORE_CRASH_AFTER=<n>  after n responses, the next request makes the
#                              fake exit 1 without answering
#   FAKE_CORE_ORPHAN=1         leave a `sleep` child holding stdout, then exit
#                              3 on the first request without answering
#   FAKE_CORE_IGNORE_EOF=1     keep running after stdin closes
#   FAKE_CORE_PID_FILE=<path>  write the fake's pid there at startup
set -u

count=0
lockdir=$(mktemp -d "${TMPDIR:-/tmp}/fake-core.XXXXXX")
lock="$lockdir/out"
trap 'rm -rf "$lockdir"' EXIT

if [ -n "${FAKE_CORE_PID_FILE:-}" ]; then
  printf '%s\n' "$$" > "$FAKE_CORE_PID_FILE"
fi
if [ "${FAKE_CORE_ORPHAN:-0}" = "1" ]; then
  sleep 5 &
fi

# One writer at a time: a split response and a background echo must not
# interleave on stdout.
write() {
  while ! mkdir "$lock" 2>/dev/null; do
    [ -d "$lockdir" ] || exit 0
    sleep 0.01
  done
  if [ "${FAKE_CORE_SPLIT:-0}" = "1" ]; then
    head=$(printf '%s' "$1" | cut -c1-12)
    tail=$(printf '%s' "$1" | cut -c13-)
    printf '%s' "$head"
    sleep 0.2
    printf '%s\n' "$tail"
  else
    printf '%s\n' "$1"
  fi
  rmdir "$lock"
}

emit() {
  write "$1"
  count=$((count + 1))
}

while IFS= read -r line; do
  if [ "${FAKE_CORE_ORPHAN:-0}" = "1" ]; then
    exit 3
  fi
  if [ -n "${FAKE_CORE_CRASH_AFTER:-}" ] && [ "$count" -ge "$FAKE_CORE_CRASH_AFTER" ]; then
    exit 1
  fi
  id=$(printf '%s' "$line" | sed -nE 's/^\{"id":([0-9]+),.*/\1/p')
  [ -n "$id" ] || id=null
  method=$(printf '%s' "$line" | sed -nE 's/^\{"id":[0-9]+,"jsonrpc":"2\.0","method":"([^"]*)".*/\1/p')
  if [ -z "$method" ]; then
    emit '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32600,"message":"fake core cannot read this request"}}'
    continue
  fi
  case "$method" in
    hello)
      version=$(printf '%s' "$line" | sed -nE 's/^.*"method":"hello","params":\{"protocol_version":([0-9]+)\}\}$/\1/p')
      if [ -z "$version" ]; then
        emit '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32602,"message":"fake core cannot read protocol_version"}}'
      elif [ "$version" = "1" ]; then
        emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocol_version":1,"core_version":"fake"}}'
      else
        emit '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":1000,"message":"core speaks protocol 1, shell sent '"$version"'","data":{"expected":1,"received":'"$version"'}}}'
        exit 65
      fi
      ;;
    host.doctor)
      emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"macos_version":"26.0","arch":"arm64","rosetta_present":true,"free_disk_bytes":123456789,"screen_recording":"unknown","developer_id_identities":0}}'
      ;;
    fake.echo)
      params=$(printf '%s' "$line" | sed -nE 's/^\{"id":[0-9]+,"jsonrpc":"2\.0","method":"fake\.echo","params":(.*)\}$/\1/p')
      if [ -z "$params" ]; then
        emit '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32600,"message":"fake core cannot read params"}}'
        continue
      fi
      delay=$(printf '%s' "$params" | sed -nE 's/.*"delay_ms":[[:space:]]*([0-9]+).*/\1/p')
      response='{"jsonrpc":"2.0","id":'"$id"',"result":'"$params"'}'
      if [ -n "$delay" ]; then
        seconds=$(awk -v ms="$delay" 'BEGIN { printf "%.3f", ms / 1000 }')
        ( sleep "$seconds"; write "$response" ) &
      else
        emit "$response"
      fi
      ;;
    *)
      emit '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":1002,"message":"not implemented","data":{"method":"'"$method"'"}}}'
      ;;
  esac
done
wait
if [ "${FAKE_CORE_IGNORE_EOF:-0}" = "1" ]; then
  sleep 30
fi
