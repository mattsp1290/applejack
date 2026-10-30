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
#   FAKE_CORE_IGNORE_EOF=1     keep running after stdin closes (SIGTERM ends it)
#   FAKE_CORE_MUTE=1           read every request and answer none of them
#   FAKE_CORE_PID_FILE=<path>  write the fake's pid there at startup
#   FAKE_CORE_IMPLEMENTED=1    answer the view-model methods (engine.*,
#                              steam.*, config.*, session.list, app.install)
#                              with canned results shaped like the Rust types;
#                              engine.install and app.install run as jobs that
#                              emit two job.progress then job.done
#   FAKE_CORE_JOB_OUTCOME=failed  the jobs end with job.failed (code 1004)
#   FAKE_CORE_JOB_OUTCOME=never   the jobs emit their progress and no terminal
#                                 notification
#   FAKE_CORE_NOTIFY_FIRST=1   engine.install writes a job.progress for job 7
#                              and one for a job 99 nobody started before the
#                              {"job_id":7} response
#   FAKE_CORE_DIE_HOLD=1       `fake.die` leaves a `sleep` child holding stdout
#                              (without it the fake just exits 3 on `fake.die`)
#   FAKE_CORE_REQUEST_LOG=<path>  append every request line to that file
#   FAKE_CORE_SESSION_LOG=<path>  the log_path session.list and app.run report
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
    # In a `( ... ) &` subshell `$$` is still the parent; a parent that was
    # killed without running its EXIT trap must not leave this loop spinning.
    { [ -d "$lockdir" ] && kill -0 $$ 2>/dev/null; } || exit 0
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

# job <id>: after the response, the notification sequence for one job.
job() {
  (
    sleep 0.05
    write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":'"$1"',"phase":"download","done_bytes":1024,"total_bytes":4096,"message":""}}'
    sleep 0.05
    write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":'"$1"',"phase":"extract","done_bytes":null,"total_bytes":null,"message":"unpacking"}}'
    sleep 0.05
    if [ "${FAKE_CORE_JOB_OUTCOME:-done}" = "never" ]; then
      :
    elif [ "${FAKE_CORE_JOB_OUTCOME:-done}" = "failed" ]; then
      write '{"jsonrpc":"2.0","method":"job.failed","params":{"job_id":'"$1"',"code":1004,"message":"checksum mismatch"}}'
    else
      write '{"jsonrpc":"2.0","method":"job.done","params":{"job_id":'"$1"'}}'
    fi
  ) &
}

# implemented <id> <method> <line>: the canned answer for a view-model
# method, or 1 when the method has none.
implemented() {
  case "$2" in
    engine.list)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"engines":[{"engine_id":"rosetta-wine","version":"10.0.1","installed":true,"active":true},{"engine_id":"rosetta-wine","version":"10.0.0","installed":true,"active":false}]}}'
      ;;
    engine.install)
      if [ "${FAKE_CORE_NOTIFY_FIRST:-0}" = "1" ]; then
        write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":99,"phase":"orphan","done_bytes":null,"total_bytes":null,"message":"nobody started this job"}}'
        write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":7,"phase":"early","done_bytes":1,"total_bytes":2,"message":"before the response"}}'
      fi
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"job_id":7}}'
      job 7
      ;;
    app.run)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"session_id":"fake-session","log_path":"'"${FAKE_CORE_SESSION_LOG:-/tmp/fake-core-session.log}"'"}}'
      ;;
    app.install)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"job_id":8}}'
      job 8
      ;;
    engine.use|engine.d3dmetal.import|steam.logout|config.set)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{}}'
      ;;
    engine.doctor)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"runnable":true,"findings":[{"severity":"info","message":"Rosetta present"},{"severity":"warning","message":"fake engine"}]}}'
      ;;
    engine.probe_cpuid)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"advertise_avx":true,"avx":true,"avx2":false,"osxsave":true}}'
      ;;
    steam.login.start)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"session_id":"login-session-fake","step":{"kind":"qr_ready","payload":"https://s.team/q/1/fake-challenge"}}}'
      ;;
    steam.login.complete)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"session_id":"login-session-fake","step":{"kind":"done","account_name":"fake-account"}}}'
      ;;
    steam.library)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"apps":[{"appid":220,"name":"Half-Life 2","drm":"steam","install_state":"installed"},{"appid":440,"name":"Team Fortress 2","drm":"unknown","install_state":"not_installed"}]}}'
      ;;
    config.get)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"engine_id":"rosetta-wine","engine_version":"10.0.1","graphics_backend":"dxmt","sync_mode":"esync","hidraw_enabled":false,"steam_input":null,"steam_overlay":true,"rosetta_advertise_avx":null,"extra_env":{"DXMT_LOG":"1"},"launch_mode":"client","launch_exe_override":null,"launch_args_override":null,"direct_verified_at":null,"isolated_prefix":false,"dx12_expectation":null,"notes":"fake"}}'
      ;;
    config.import)
      case "$3" in
        *'"accept_launch_overrides":true'*)
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"applied":true,"accepted":[{"field":"graphics_backend","value":"dxmt"},{"field":"launch_exe_override","value":"bin/game.exe"}],"rejected":[{"field":"extra_env.VK_ICD_FILENAMES","value":"/tmp/x.json","reason":"path outside the engine roots"}],"confirmations":[{"field":"launch_exe_override","value":"bin/game.exe"}]}}'
          ;;
        *)
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"applied":false,"accepted":[{"field":"graphics_backend","value":"dxmt"}],"rejected":[{"field":"extra_env.VK_ICD_FILENAMES","value":"/tmp/x.json","reason":"path outside the engine roots"}],"confirmations":[{"field":"launch_exe_override","value":"bin/game.exe"}]}}'
          ;;
      esac
      ;;
    session.list)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"sessions":[{"id":1,"appid":220,"started_at":"2026-10-30T00:00:00Z","ended_at":null,"engine_id":"rosetta-wine","engine_version":"10.0.1","exit_code":null,"log_path":"'"${FAKE_CORE_SESSION_LOG:-/tmp/fake-core-session.log}"'"}]}}'
      ;;
    *)
      return 1
      ;;
  esac
}

while IFS= read -r line; do
  if [ -n "${FAKE_CORE_REQUEST_LOG:-}" ]; then
    printf '%s\n' "$line" >> "$FAKE_CORE_REQUEST_LOG"
  fi
  if [ "${FAKE_CORE_MUTE:-0}" = "1" ]; then
    continue
  fi
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
    fake.die)
      # Exits 3 without answering. The grandchild inherits stdout only (an
      # asynchronous list reads /dev/null), so the next write to stdin fails
      # while stdout has not reached EOF.
      if [ "${FAKE_CORE_DIE_HOLD:-0}" = "1" ]; then
        sleep 3 &
      fi
      exit 3
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
      if [ "${FAKE_CORE_IMPLEMENTED:-0}" = "1" ] && implemented "$id" "$method" "$line"; then
        continue
      fi
      emit '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":1002,"message":"`'"$method"'` is pending (fake core)","data":{"method":"'"$method"'"}}}'
      ;;
  esac
done
wait
if [ "${FAKE_CORE_IGNORE_EOF:-0}" = "1" ]; then
  # `Process.terminate()` signals this shell only; take the sleep down with it.
  sleep 30 &
  child=$!
  trap 'kill "$child" 2>/dev/null; rm -rf "$lockdir"; exit 0' TERM
  wait "$child"
fi
