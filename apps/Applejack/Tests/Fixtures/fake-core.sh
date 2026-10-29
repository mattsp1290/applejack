#!/bin/sh
# Stand-in for `applejack serve --stdio` used by CoreClientTests.
#
# Answers `hello` (protocol 1 only; anything else gets ProtocolMismatch and
# exit 65), `host.doctor` (canned facts), and `fake.echo` (returns its params;
# a `delay_ms` member delays that one answer in the background so a later
# request is answered first). Every other method gets NotImplemented.
#
#   FAKE_CORE_SPLIT=1          write each response in two chunks with a pause
#   FAKE_CORE_CRASH_AFTER=<n>  after n responses, the next request makes the
#                              fake exit 1 without answering
set -u

count=0

emit() {
  if [ "${FAKE_CORE_SPLIT:-0}" = "1" ]; then
    head=$(printf '%s' "$1" | cut -c1-12)
    tail=$(printf '%s' "$1" | cut -c13-)
    printf '%s' "$head"
    sleep 0.2
    printf '%s\n' "$tail"
  else
    printf '%s\n' "$1"
  fi
  count=$((count + 1))
}

while IFS= read -r line; do
  if [ -n "${FAKE_CORE_CRASH_AFTER:-}" ] && [ "$count" -ge "$FAKE_CORE_CRASH_AFTER" ]; then
    exit 1
  fi
  id=$(printf '%s' "$line" | sed -E 's/.*"id":[[:space:]]*([0-9]+).*/\1/')
  method=$(printf '%s' "$line" | sed -E 's/.*"method":[[:space:]]*"([^"]*)".*/\1/')
  case "$method" in
    hello)
      version=$(printf '%s' "$line" | sed -E 's/.*"protocol_version":[[:space:]]*([0-9]+).*/\1/')
      if [ "$version" = "1" ]; then
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
      params=$(printf '%s' "$line" | sed -E 's/^.*"params":(.*)\}$/\1/')
      delay=$(printf '%s' "$params" | sed -nE 's/.*"delay_ms":[[:space:]]*([0-9]+).*/\1/p')
      response='{"jsonrpc":"2.0","id":'"$id"',"result":'"$params"'}'
      if [ -n "$delay" ]; then
        seconds=$(awk -v ms="$delay" 'BEGIN { printf "%.3f", ms / 1000 }')
        ( sleep "$seconds"; printf '%s\n' "$response" ) &
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
