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
#                              steam.*, config.*, session.list, app.install,
#                              runtime.steam.*) with canned results shaped
#                              like the Rust types; engine.install,
#                              app.install, steam.library with refresh, and
#                              the runtime.steam operations run as jobs that
#                              emit two job.progress then job.done
#   FAKE_CORE_LOGIN_POLLS=<n>  steam.login.complete answers `waiting` n times
#                              before `done`; with FAKE_CORE_LOGIN_GUARD a poll
#                              without a code answers guard_required instead,
#                              as the core's poll at AwaitingGuardCode does,
#                              until a code is accepted (n = 0 stands in for
#                              the mobile app's approval and answers `done`)
#   FAKE_CORE_LOGIN_ROTATE=1   each steam.login.start answers a new session id,
#                              and steam.login.complete answers the latest
#   FAKE_CORE_JOB_OUTCOME=failed  the jobs end with job.failed (code 1004)
#   FAKE_CORE_JOB_OUTCOME=never   the jobs emit their progress and no terminal
#                                 notification
#   FAKE_CORE_NOTIFY_FIRST=1   engine.install writes a job.progress for job 7
#                              and one for a job 99 nobody started before the
#                              {"job_id":7} response
#   FAKE_CORE_DIE_HOLD=1       `fake.die` leaves a `sleep` child holding stdout
#                              (without it the fake just exits 3 on `fake.die`)
#   FAKE_CORE_BAD_HELLO=malformed  answer hello with a result HelloResult cannot
#                                  decode; =error answers it with an Internal
#                                  rpc error
#   FAKE_CORE_RUN_DELAY_MS=<n>  answer app.run after n milliseconds, in the
#                               background
#   FAKE_CORE_REQUEST_LOG=<path>  append every request line to that file
#   FAKE_CORE_SESSION_LOG=<path>  the log_path session.list and app.run report
#   FAKE_CORE_RUN_OUTCOME=exits   app.run's job (12) ends with job.done after
#                                 `running`; without it the job stays open
#                                 until app.cancel ends it
#   FAKE_CORE_CANCEL_REFUSED=1    app.cancel answers NotRunning (1017) for an
#                                 open row too
#   FAKE_CORE_CANCEL_CLOSE_MS=<n>  app.cancel's job (13) closes the row n
#                                 milliseconds after its `cancel` phase
#                                 instead of 50, so a title's exit can land
#                                 first
#   FAKE_CORE_RUNTIME_STATE=<s>   runtime.steam.status answers state <s>
#                                 (absent|ready|broken; default needs_login)
#   FAKE_CORE_RUNTIME_RUNNING=<0|1>  runtime.steam.status answers that
#                                 `running` whatever the state says
#   FAKE_CORE_RUNTIME_OUTCOME=broken  the runtime.steam jobs end with
#                                 job.failed 1034 after their `start:` line
#   FAKE_CORE_RESET_OUTCOME=failed  prefix.reset ends with job.failed 1003
#                                 (Internal: the code of a wineboot that failed
#                                 inside the rebuild) naming the safety
#                                 snapshot to restore
#   FAKE_CORE_RESET_SNAPSHOT=<name>  the safety snapshot prefix.reset names
#                                 (default 20261103T140000Z; the core names
#                                 a second snapshot in one second <ts>-001)
#   FAKE_CORE_JOB_DELAY_MS=<n>    answer runtime.steam.* and prefix.reset
#                                 after n milliseconds, in the background
#   FAKE_CORE_LOGIN_GUARD=<factor>  steam.login.start answers guard_required
#                                 with that factor instead of qr_ready
#   FAKE_CORE_LOGIN_FAIL=1        steam.login.complete answers
#                                 SteamLoginFailed (1021)
#   FAKE_CORE_LOGIN_REJECT_ONCE=1  the first steam.login.complete carrying a
#                                 steam_guard_code answers SteamGuardRejected
#                                 (1022); the next one with a code answers done
#   FAKE_CORE_CONTROLLERS=<shape>  host.doctor's `controllers`: `empty` (the
#                                 default), `dualsense` (one connected over
#                                 Bluetooth), `xbox` (one wired), `dualsense-away`
#                                 (paired, not connected), `unavailable` (no
#                                 pads and `controllers_unavailable` set, the
#                                 shape of a profiler report that timed out),
#                                 `refuse` (host.doctor answers Internal 1003
#                                 instead), or a JSON array in the Rust shape,
#                                 used as given
#   FAKE_CORE_HIDRAW=<unset|true|false>  config.get's `hidraw_enabled` (default
#                                 false); config.resolve answers the stored
#                                 value from `config`, or with `unset` the
#                                 rule FAKE_CORE_CONTROLLERS gives from
#                                 `controllers`, as `HidrawDecision` does
#                                 (`unavailable`: the prefix's mode, `false`
#                                 for one not built yet, and the reason)
#
# session.list reflects what happened, as the core's `sessions` row does: the
# canned open row 1 (running) until app.run, then the run's row 41, `starting`
# from the request and `running` from the job's `running` phase (about 100 ms
# after the answer here, where the core takes seconds to ensure the prefix
# and spawn the title); app.cancel sets `cancel_requested` at the request and
# its job closes the row `cancelled`; the title's exit
# (FAKE_CORE_RUN_OUTCOME=exits) closes it `exited`, or `cancelled` when a
# cancel was asked for first, since the core's owner closes a row by the flag
# (`PlannedRun::close`). A row closed while its owner was alive carries the
# status the owner reaped (`WatchEnd.exit_code`, whatever the state): 0 for a
# title that exited on its own, 143 for one the cancel ended (`end_title`
# sends SIGTERM and the title leaves on it well inside `term_wait`;
# `prefixes::exit_code` maps signal 15 to 128 + 15). Only a close with no
# owner alive, the cancel's own (`PlannedCancel::run`, `NOTE_LOST`), records
# none; row 1 is such a row here. The flag is read and the row closed under
# one lock, the cancel request flags the row under it, and session.list reads
# under it, so a cancel cannot land between the check and the close and a
# reader never sees a row half written. A cancel without a run closes row 1.
# `app.run` over an open row 41 answers AlreadyRunning (1014) and
# `app.cancel` on a closed row NotRunning (1017), as `plan_run` and
# `plan_cancel` do. The row is a state machine, `starting -> running ->
# {cancelled | exited}` (or `starting -> cancelled` when the cancel's close
# lands before the running marker), and a terminal state never changes: the
# core ends a row once (`end_session`) and never reopens it, so a run job
# that wakes late must not put `running` over a `cancelled` (a repeated
# `running` is accepted here and a no-op in the core). Each run carries a
# serial, stamped at `starting`; a job of an earlier run writes with the
# serial it was born with and is refused once a later run replaced the row,
# as the core's `end_session(id)` and `mark_session_running(id)` are by row
# id and never touch the next row.
set -u
# A writer whose reader is gone must release the lock, not die holding it:
# with SIGPIPE ignored the write fails with EPIPE and `write` ends the
# subshell after `rmdir`.
trap '' PIPE

count=0
lockdir=$(mktemp -d "${TMPDIR:-/tmp}/fake-core.XXXXXX")
lock="$lockdir/out"
trap 'rm -rf "$lockdir"' EXIT

if [ -n "${FAKE_CORE_PID_FILE:-}" ]; then
  printf '%s\n' "$$" > "$FAKE_CORE_PID_FILE"
  # A SIGKILL runs no EXIT trap; a test that kills by pid can sweep the lock
  # directory itself.
  printf '%s\n' "$lockdir" > "$FAKE_CORE_PID_FILE.lockdir"
fi
if [ "${FAKE_CORE_ORPHAN:-0}" = "1" ]; then
  sleep 5 &
fi

# A line that will never reach the shell, said where the test log shows it.
# Ends the writer (a `( ... ) &` subshell, or the fake itself) with 1.
write_failed() {
  printf 'fake-core: %s\n' "$1" >&2
  exit 1
}

# acquire <lock dir> <what is dropped>: a bounded mkdir lock shared by the
# subshells and the fake itself; a holder that keeps it for 500 tries (about
# 11 s unloaded, each try a `sleep` spawn) is a bug, and the waiter says what
# it drops.
acquire() {
  spins=0
  while ! mkdir "$1" 2>/dev/null; do
    # In a `( ... ) &` subshell `$$` is still the parent; a parent that was
    # killed without running its EXIT trap must not leave this loop spinning,
    # and neither must a lock a dead writer left behind.
    { [ -d "$lockdir" ] && kill -0 $$ 2>/dev/null; } || write_failed "the fake is gone; dropping: $2"
    spins=$((spins + 1))
    [ "$spins" -lt 500 ] || write_failed "the lock $(basename "$1") was not released in 500 tries; dropping: $2"
    sleep 0.01
  done
}

# One writer at a time: a split response and a background echo must not
# interleave on stdout. The only silent exit is a reader that is gone
# (EPIPE); every other failure is a write_failed.
write() {
  acquire "$lock" "$1"
  written=0
  error="$lockdir/write-error"
  : > "$error"
  if [ "${FAKE_CORE_SPLIT:-0}" = "1" ]; then
    head=$(printf '%s' "$1" | cut -c1-12)
    tail=$(printf '%s' "$1" | cut -c13-)
    printf '%s' "$head" 2>>"$error" || written=1
    sleep 0.2
    printf '%s\n' "$tail" 2>>"$error" || written=1
  else
    printf '%s\n' "$1" 2>>"$error" || written=1
  fi
  rmdir "$lock"
  [ "$written" = "0" ] && return 0
  # The reader is gone: nothing this subshell could still say would arrive.
  if grep -q 'Broken pipe' "$error" 2>/dev/null; then
    exit 0
  fi
  write_failed "write failed ($(cat "$error" 2>/dev/null)); dropping: $1"
}

# set_run_state <state> <serial>: the state of the run's row 41, replaced
# whole and under the lock session.list reads under. `starting` inserts the
# row (a new run after the last one ended) with no flag and no reaped exit,
# stamped with the run's serial; any other write is refused with 1 against a
# terminal state, or when the row's serial is not the writer's (a job of an
# earlier run, whose row a later run replaced), under a lock so the run job
# and the cancel job cannot interleave their read-check-replace. `ended` is
# the title's exit: the owner closes by the flag (`PlannedRun::close`), read
# here under the lock app.cancel flags under, so a cancel cannot land between
# the check and the write; the `reaped` file carries the status the owner
# reaped, 0 for the exit, 143 for `cancelled` from the cancel job (the title
# left on the SIGTERM). In `ended` mode the state chosen is printed, so call
# it as `$(set_run_state ended "$serial")`.
set_run_state() {
  acquire "$lockdir/run.lock" "run state $1"
  current=; serial=
  [ -f "$lockdir/run" ] && read -r current < "$lockdir/run"
  [ -f "$lockdir/run.serial" ] && read -r serial < "$lockdir/run.serial"
  state=$1
  if [ "$state" = ended ]; then
    state=exited
    [ -f "$lockdir/cancel_requested" ] && state=cancelled
  fi
  changed=0
  if [ "$state" = starting ] || [ "${2:-}" = "$serial" ]; then
    case "$state:$current" in
      starting:*|*:|*:starting|*:running)
        if [ "$state" = starting ]; then
          rm -f "$lockdir/cancel_requested" "$lockdir/reaped"
          printf '%s\n' "$2" > "$lockdir/run.serial"
        fi
        case "$1" in
          ended) printf '%s\n' 0 > "$lockdir/reaped" ;;
          cancelled) printf '%s\n' 143 > "$lockdir/reaped" ;;
        esac
        printf '%s\n' "$state" > "$lockdir/run.next" && mv -f "$lockdir/run.next" "$lockdir/run"
        changed=1
        ;;
    esac
  fi
  rmdir "$lockdir/run.lock"
  [ "$changed" = "1" ] || return 1
  [ "$1" = ended ] && printf '%s\n' "$state"
  return 0
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

# runtime_job <id>: the notification sequence of a runtime.steam job, whose
# `start` line is the one the shell must not swallow.
runtime_job() {
  (
    sleep 0.05
    write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":'"$1"',"phase":"prefix","done_bytes":null,"total_bytes":null,"message":"claimed steam-client"}}'
    sleep 0.05
    write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":'"$1"',"phase":"start","done_bytes":null,"total_bytes":null,"message":"macOS may ask whether \"applejack\" may use the microphone within about 15 s of the client'"'"'s start: answer it (Don'"'"'t Allow is fine); left unanswered, the client stalls and dies about 3 minutes in"}}'
    sleep 0.05
    if [ "${FAKE_CORE_RUNTIME_OUTCOME:-done}" = "broken" ]; then
      write '{"jsonrpc":"2.0","method":"job.failed","params":{"job_id":'"$1"',"code":1034,"message":"the Steam client never reached the login window; run `applejack runtime steam repair`"}}'
    else
      write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":'"$1"',"phase":"status","done_bytes":null,"total_bytes":null,"message":"ready"}}'
      sleep 0.05
      write '{"jsonrpc":"2.0","method":"job.done","params":{"job_id":'"$1"'}}'
    fi
  ) &
}

# run_job <seconds> <serial>: app.run's job 12, the session's phases, after
# the answer (delayed by <seconds> with FAKE_CORE_RUN_DELAY_MS); the row is
# `running` from the job's `running` phase, as the core's is from the marker.
# The job stays open until app.cancel ends it unless FAKE_CORE_RUN_OUTCOME
# says the title exits. A cancel that closed the row first also ended job
# 12, and a done job says nothing more (CoreClient's forwarder drops a line
# after job.done anyway), so a refused state write sends no progress line.
run_job() {
  (
    sleep "${1:-0}"
    sleep 0.05
    write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":12,"phase":"starting","done_bytes":null,"total_bytes":null,"message":""}}'
    sleep 0.05
    set_run_state running "$2" || exit 0
    write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":12,"phase":"running","done_bytes":null,"total_bytes":null,"message":""}}'
    if [ "${FAKE_CORE_RUN_OUTCOME:-open}" = "exits" ]; then
      sleep 0.05
      # The title left: the owner closes the row by the flag, under the lock.
      end=$(set_run_state ended "$2") || exit 0
      write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":12,"phase":"'"$end"'","done_bytes":null,"total_bytes":null,"message":"session 41 ended"}}'
      write '{"jsonrpc":"2.0","method":"job.done","params":{"job_id":12}}'
    fi
  ) &
}

# session_row: the one row session.list answers, as the run and cancel
# requests and their jobs left it (see the header), read under the lock the
# writers take so a row is never seen half written.
session_row() {
  acquire "$lockdir/run.lock" "session.list"
  id=1; state=running; ended=null; exit_code=null; cancel_requested=false
  if [ -f "$lockdir/run" ]; then
    id=41
    read -r state < "$lockdir/run"
  elif [ -f "$lockdir/cancelled" ]; then
    state=cancelled
  fi
  [ -f "$lockdir/cancel_requested" ] && cancel_requested=true
  case "$state" in
    cancelled|exited)
      ended='"2026-10-30T00:01:00Z"'
      # Closed by an owner that was alive (the exit, or a cancel it reaped
      # the title for): the launch exe's status. Closed with no owner (row
      # 1, a dead owner's row: `PlannedCancel::run`, `NOTE_LOST`): none.
      [ -f "$lockdir/reaped" ] && read -r exit_code < "$lockdir/reaped"
      ;;
  esac
  rmdir "$lockdir/run.lock"
  printf '%s' '{"id":'"$id"',"appid":220,"started_at":"2026-10-30T00:00:00Z","ended_at":'"$ended"',"engine_id":"rosetta-wine","engine_version":"10.0.1","exit_code":'"$exit_code"',"log_path":"'"${FAKE_CORE_SESSION_LOG:-/tmp/fake-core-session.log}"'","state":"'"$state"'","session_key":"fake-session","prefix":"220","launch_exe":"hl2.exe","note":null,"cancel_requested":'"$cancel_requested"'}'
}

# delayed <response> <job function> <job id>: the answer after
# FAKE_CORE_JOB_DELAY_MS in the background, or at once without it; the
# job's notifications follow either way.
delayed() {
  if [ -n "${FAKE_CORE_JOB_DELAY_MS:-}" ]; then
    seconds=$(awk -v ms="$FAKE_CORE_JOB_DELAY_MS" 'BEGIN { printf "%.3f", ms / 1000 }')
    ( sleep "$seconds"; write "$1" ) &
  else
    emit "$1"
  fi
  "$2" "$3"
}

# reset_job <prefix>: prefix.reset's job 14. A failure carries the code and
# the text of its cause, as the core's ResetFailed does: a wineboot that
# fails inside the rebuild is WineCommandFailed, which `engine_code` maps
# to Internal (1003). The message names the safety snapshot to restore.
reset_job() {
  (
    sleep 0.05
    write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":14,"phase":"reset","done_bytes":null,"total_bytes":null,"message":"rebuilding prefix '"$1"'"}}'
    sleep 0.05
    if [ "${FAKE_CORE_RESET_OUTCOME:-done}" = "failed" ]; then
      write '{"jsonrpc":"2.0","method":"job.failed","params":{"job_id":14,"code":1003,"message":"resetting prefix '"$1"' failed; `applejack prefix restore '"$1"' '"${FAKE_CORE_RESET_SNAPSHOT:-20261103T140000Z}"'` brings it back: `/data/engines/rosetta-wine/10.0.1/bin/wine64 wineboot --init` failed (exit status: 1); log at `/data/logs/prefixes/'"$1"'/wineboot.log`"}}'
    else
      write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":14,"phase":"reset","done_bytes":null,"total_bytes":null,"message":"safety snapshot '"${FAKE_CORE_RESET_SNAPSHOT:-20261103T140000Z}"'"}}'
      sleep 0.05
      write '{"jsonrpc":"2.0","method":"job.done","params":{"job_id":14}}'
    fi
  ) &
}

# cancel_job <end run job> <serial>: app.cancel's job 13; it closes the row
# `cancelled` before it reports so, and with a run open, job 12 reports its
# end state first: the cancel waits on the owner's lock, and the owner
# closes the row with the status it reaped from the title the cancel ended
# and reports it before it lets go (`PlannedCancel::run`, `PlannedRun::close`),
# so the close here stands in for the owner's and leaves the reaped status;
# job 12's `job.done` follows the lock drop, so it is written here before
# job 13's lines as the likely order, not a promised one. A row the title's
# exit closed first stays as the exit left it, and job 12 already ended; a
# row a later run replaced is not this job's and is left alone.
cancel_job() {
  (
    sleep 0.05
    write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":13,"phase":"cancel","done_bytes":null,"total_bytes":null,"message":"SIGTERM sent"}}'
    if [ -n "${FAKE_CORE_CANCEL_CLOSE_MS:-}" ]; then
      sleep "$(awk -v ms="$FAKE_CORE_CANCEL_CLOSE_MS" 'BEGIN { printf "%.3f", ms / 1000 }')"
    else
      sleep 0.05
    fi
    ends_run=0
    row=1
    if [ "$1" = "1" ]; then
      row=41
      set_run_state cancelled "$2" && ends_run=1
    else
      : > "$lockdir/cancelled"
    fi
    if [ "$ends_run" = "1" ]; then
      write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":12,"phase":"cancelled","done_bytes":null,"total_bytes":null,"message":"session 41 ended"}}'
      write '{"jsonrpc":"2.0","method":"job.done","params":{"job_id":12}}'
    fi
    write '{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":13,"phase":"cancelled","done_bytes":null,"total_bytes":null,"message":"session '"$row"' ended"}}'
    write '{"jsonrpc":"2.0","method":"job.done","params":{"job_id":13}}'
  ) &
}

# controllers_json: host.doctor's `controllers` per FAKE_CORE_CONTROLLERS,
# in the shape `host.doctor.result.json` pins (ids lowercase `0x` hex). The
# `unavailable` shape also carries the `controllers_unavailable` member the
# core adds only when a report failed.
controllers_json() {
  case "${FAKE_CORE_CONTROLLERS:-empty}" in
    empty) printf '%s' '[]' ;;
    dualsense) printf '%s' '[{"name":"DualSense Wireless Controller","transport":"bluetooth","vendor_id":"0x054c","product_id":"0x0ce6","connected":true,"kind":"dualsense"}]' ;;
    dualsense-away) printf '%s' '[{"name":"DualSense Wireless Controller","transport":"bluetooth","vendor_id":"0x054c","product_id":"0x0ce6","connected":false,"kind":"dualsense"}]' ;;
    unavailable) printf '%s' '[],"controllers_unavailable":"SPBluetoothDataType timed out"' ;;
    xbox) printf '%s' '[{"name":"Xbox Wireless Controller","transport":"usb","vendor_id":"0x045e","product_id":"0x0b12","connected":true,"kind":"xbox"}]' ;;
    *) printf '%s' "$FAKE_CORE_CONTROLLERS" ;;
  esac
}

# hidraw_json: config.get's `hidraw_enabled` per FAKE_CORE_HIDRAW.
hidraw_json() {
  case "${FAKE_CORE_HIDRAW:-false}" in
    unset) printf '%s' null ;;
    true) printf '%s' true ;;
    *) printf '%s' false ;;
  esac
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
      # The core's `sessions_one_open` index refuses a second open row
      # (AlreadyRunning); a new run here reuses row 41 where the core would
      # insert a new id, and the canned row 1 gives way to the first run
      # where the core would refuse it.
      if [ -f "$lockdir/run" ] && read -r current < "$lockdir/run" && { [ "$current" = starting ] || [ "$current" = running ]; }; then
        emit '{"jsonrpc":"2.0","id":'"$1"',"error":{"code":1014,"message":"app 220 already has an open session"}}'
        return 0
      fi
      response='{"jsonrpc":"2.0","id":'"$1"',"result":{"id":41,"job_id":12,"log_path":"'"${FAKE_CORE_SESSION_LOG:-/tmp/fake-core-session.log}"'"}}'
      run_open=1
      run_serial=$((${run_serial:-0} + 1))
      # The core inserts the row, `starting`, before it answers.
      set_run_state starting "$run_serial"
      seconds=0
      if [ -n "${FAKE_CORE_RUN_DELAY_MS:-}" ]; then
        seconds=$(awk -v ms="$FAKE_CORE_RUN_DELAY_MS" 'BEGIN { printf "%.3f", ms / 1000 }')
        ( sleep "$seconds"; write "$response" ) &
      else
        emit "$response"
      fi
      run_job "$seconds" "$run_serial"
      ;;
    app.cancel)
      # `plan_cancel` reads the open row and flags it at the request in two
      # statements; `request_session_cancel`'s `WHERE ended_at IS NULL` keeps
      # a closed row unflagged. Here the read and the flag go under the run
      # lock, which also refuses the one pair the core can still show in
      # that window (`exited` with the flag set); its job closes the row.
      acquire "$lockdir/run.lock" "cancel of row 41"
      closed=0
      if [ -f "$lockdir/run" ] && read -r current < "$lockdir/run"; then
        case "$current" in cancelled|exited) closed=1 ;; esac
      elif [ -f "$lockdir/cancelled" ]; then
        closed=1
      fi
      refused=0
      if [ "${FAKE_CORE_CANCEL_REFUSED:-0}" = "1" ] || [ "$closed" = "1" ]; then
        refused=1
      else
        : > "$lockdir/cancel_requested"
      fi
      rmdir "$lockdir/run.lock"
      if [ "$refused" = "1" ]; then
        emit '{"jsonrpc":"2.0","id":'"$1"',"error":{"code":1017,"message":"app 220 has no open session"}}'
      else
        emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"job_id":13}}'
        cancel_job "${run_open:-0}" "${run_serial:-0}"
        run_open=0
      fi
      ;;
    config.export)
      path=$(printf '%s' "$3" | sed -nE 's/.*"path":"([^"]*)".*/\1/p')
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"path":"'"$path"'"}}'
      ;;
    prefix.snapshot)
      prefix=$(printf '%s' "$3" | sed -nE 's/.*"prefix":"([^"]*)".*/\1/p')
      snapshot_serial=$((${snapshot_serial:-0} + 1))
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"prefix":"'"$prefix"'","timestamp":"20261103T12000'"$snapshot_serial"'Z","dir":"/data/prefixes/.snapshots/'"$prefix"'/20261103T12000'"$snapshot_serial"'Z"}}'
      ;;
    prefix.restore)
      prefix=$(printf '%s' "$3" | sed -nE 's/.*"prefix":"([^"]*)".*/\1/p')
      timestamp=$(printf '%s' "$3" | sed -nE 's/.*"timestamp":"([^"]*)".*/\1/p')
      # The real core answers InvalidParams (SnapshotNotFound) for a name it never made.
      case "$timestamp" in
        [0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]T[0-9][0-9][0-9][0-9][0-9][0-9]Z|[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]T[0-9][0-9][0-9][0-9][0-9][0-9]Z-[0-9][0-9][0-9])
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"prefix":"'"$prefix"'","timestamp":"20261103T130000Z","dir":"/data/prefixes/.snapshots/'"$prefix"'/20261103T130000Z"}}'
          ;;
        *)
          emit '{"jsonrpc":"2.0","id":'"$1"',"error":{"code":-32602,"message":"snapshot '"$prefix"'/'"$timestamp"' does not exist"}}'
          ;;
      esac
      ;;
    prefix.reset)
      prefix=$(printf '%s' "$3" | sed -nE 's/.*"prefix":"([^"]*)".*/\1/p')
      delayed '{"jsonrpc":"2.0","id":'"$1"',"result":{"job_id":14}}' reset_job "$prefix"
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
      if [ -n "${FAKE_CORE_LOGIN_ROTATE:-}" ]; then
        login_serial=$((${login_serial:-0} + 1))
        login_session="login-session-fake-$login_serial"
      fi
      if [ -n "${FAKE_CORE_LOGIN_GUARD:-}" ]; then
        emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"session_id":"'"${login_session:-login-session-fake}"'","step":{"kind":"guard_required","factor":"'"$FAKE_CORE_LOGIN_GUARD"'"}}}'
      else
        emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"session_id":"'"${login_session:-login-session-fake}"'","step":{"kind":"qr_ready","payload":"https://s.team/q/1/fake-challenge"}}}'
      fi
      ;;
    steam.login.complete)
      if [ "${FAKE_CORE_LOGIN_FAIL:-0}" = "1" ]; then
        emit '{"jsonrpc":"2.0","id":'"$1"',"error":{"code":1021,"message":"the login is over; begin again"}}'
        return 0
      fi
      if [ "${FAKE_CORE_LOGIN_REJECT_ONCE:-0}" = "1" ]; then
        case "$3" in
          *'"steam_guard_code":'*)
            if [ "${guard_rejected:-0}" = "0" ]; then
              guard_rejected=1
              emit '{"jsonrpc":"2.0","id":'"$1"',"error":{"code":1022,"message":"Steam Guard code rejected"}}'
            else
              guard_accepted=1
              emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"session_id":"'"${login_session:-login-session-fake}"'","step":{"kind":"done","account_name":"fake-account"}}}'
            fi
            return 0
            ;;
        esac
      fi
      case "$3" in
        *'"steam_guard_code":'*) guard_accepted=1 ;;
      esac
      case "${FAKE_CORE_LOGIN_POLLS:-0}" in
        0) emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"session_id":"'"${login_session:-login-session-fake}"'","step":{"kind":"done","account_name":"fake-account"}}}' ;;
        *)
          if [ -n "${FAKE_CORE_LOGIN_GUARD:-}" ] && [ "${guard_accepted:-0}" = "0" ]; then
            # `Authenticator::poll` at AwaitingGuardCode answers GuardRequired
            # (auth.rs); only a code that goes through moves the step on.
            emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"session_id":"'"${login_session:-login-session-fake}"'","step":{"kind":"guard_required","factor":"'"$FAKE_CORE_LOGIN_GUARD"'"}}}'
          else
            FAKE_CORE_LOGIN_POLLS=$((FAKE_CORE_LOGIN_POLLS - 1))
            emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"session_id":"'"${login_session:-login-session-fake}"'","step":{"kind":"waiting"}}}'
          fi
          ;;
      esac
      ;;
    steam.library)
      apps='[{"appid":220,"name":"Half-Life 2","installdir":"Half-Life 2","size_bytes":4096,"drm":"steam","default_branch":"public","install_state":"installed"},{"appid":440,"name":"Team Fortress 2","installdir":"Team Fortress 2","size_bytes":8192,"drm":"unknown","default_branch":"public","install_state":"not_installed"}]'
      case "$3" in
        *'"refresh":true'*)
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"apps":'"$apps"',"fetched_at":"2026-11-02T00:00:00Z","job_id":9}}'
          job 9
          ;;
        *)
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"apps":'"$apps"',"fetched_at":"2026-11-02T00:00:00Z","job_id":null}}'
          ;;
      esac
      ;;
    config.resolve)
      # `HidrawDecision::decide`: a stored value wins; unset, a connected
      # Sony pad disables, else enabled. The named shapes carry one pad at
      # most, so the DualSense is the only Sony culprit the fake can name.
      case "${FAKE_CORE_HIDRAW:-false}" in
        unset)
          case "${FAKE_CORE_CONTROLLERS:-empty}" in
            dualsense) emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"appid":220,"hidraw_enabled":false,"hidraw_reason":"DualSense connected","hidraw_source":"controllers"}}' ;;
            unavailable) emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"appid":220,"hidraw_enabled":false,"hidraw_reason":"controller inventory unavailable (SPBluetoothDataType timed out)","hidraw_source":"controllers"}}' ;;
            *) emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"appid":220,"hidraw_enabled":true,"hidraw_reason":"no Sony pad connected","hidraw_source":"controllers"}}' ;;
          esac
          ;;
        *) emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"appid":220,"hidraw_enabled":'"$(hidraw_json)"',"hidraw_reason":"config","hidraw_source":"config"}}' ;;
      esac
      ;;
    config.get)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"engine_id":"rosetta-wine","engine_version":"10.0.1","graphics_backend":"dxmt","sync_mode":"esync","hidraw_enabled":'"$(hidraw_json)"',"steam_input":null,"steam_overlay":true,"rosetta_advertise_avx":null,"extra_env":{"DXMT_LOG":"1"},"launch_mode":"client","launch_exe_override":null,"launch_args_override":null,"direct_verified_at":null,"isolated_prefix":false,"dx12_expectation":null,"notes":"fake"}}'
      ;;
    config.import)
      # A path containing "foreign" is a preset exported for another title.
      case "$3" in
        *foreign*) preset=730 ;;
        *) preset=220 ;;
      esac
      case "$3" in
        # A path containing "slow" previews after 400 ms, so a later preset's
        # preview can overtake it (lifted from the round-two review probe).
        *'"accept_launch_overrides":false'*slow*)
          ( sleep 0.4; write '{"jsonrpc":"2.0","id":'"$1"',"result":{"applied":false,"preset_appid":220,"accepted":[],"rejected":[],"confirmations":[{"field":"launch_exe_override","value":"slow.exe"}]}}' ) &
          ;;
        *'"accept_launch_overrides":true'*)
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"applied":true,"preset_appid":'"$preset"',"accepted":[{"field":"graphics_backend","value":"dxmt"},{"field":"launch_exe_override","value":"bin/game.exe"}],"rejected":[{"field":"extra_env.VK_ICD_FILENAMES","value":"/tmp/x.json","reason":"path outside the engine roots"}],"confirmations":[{"field":"launch_exe_override","value":"bin/game.exe"}]}}'
          ;;
        *)
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"applied":false,"preset_appid":'"$preset"',"accepted":[{"field":"graphics_backend","value":"dxmt"}],"rejected":[{"field":"extra_env.VK_ICD_FILENAMES","value":"/tmp/x.json","reason":"path outside the engine roots"}],"confirmations":[{"field":"launch_exe_override","value":"bin/game.exe"}]}}'
          ;;
      esac
      ;;
    session.list)
      emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"sessions":['"$(session_row)"']}}'
      ;;
    runtime.steam.status)
      state=${FAKE_CORE_RUNTIME_STATE:-needs_login}
      case "$state" in
        absent|ready|broken) running=false ;;
        *) running=true ;;
      esac
      case "${FAKE_CORE_RUNTIME_RUNNING:-}" in
        1) running=true ;;
        0) running=false ;;
      esac
      case "$state" in
        absent)
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"state":"absent","prefix":"/data/prefixes/steam-client","build_id":null,"running":'"$running"',"snapshots":0,"detail":"no steam-client prefix"}}'
          ;;
        ready)
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"state":"ready","prefix":"/data/prefixes/steam-client","build_id":"1761720601","running":'"$running"',"snapshots":2,"detail":""}}'
          ;;
        broken)
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"state":"broken","prefix":"/data/prefixes/steam-client","build_id":"1761720601","running":'"$running"',"snapshots":2,"detail":"the client never reached the login window; `runtime.steam.repair` restores the newest usable snapshot"}}'
          ;;
        *)
          emit '{"jsonrpc":"2.0","id":'"$1"',"result":{"state":"needs_login","prefix":"/data/prefixes/steam-client","build_id":"1761720601","running":'"$running"',"snapshots":1,"detail":"no account is signed in"}}'
          ;;
      esac
      ;;
    runtime.steam.install|runtime.steam.update|runtime.steam.repair|runtime.steam.stop)
      delayed '{"jsonrpc":"2.0","id":'"$1"',"result":{"job_id":10}}' runtime_job 10
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
      elif [ "${FAKE_CORE_BAD_HELLO:-}" = "malformed" ]; then
        emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"nope":1}}'
      elif [ "${FAKE_CORE_BAD_HELLO:-}" = "error" ]; then
        emit '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32603,"message":"fake core refuses to be greeted"}}'
      elif [ "$version" = "1" ]; then
        emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocol_version":1,"core_version":"fake"}}'
      else
        emit '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":1000,"message":"core speaks protocol 1, shell sent '"$version"'","data":{"expected":1,"received":'"$version"'}}}'
        exit 65
      fi
      ;;
    host.doctor)
      if [ "${FAKE_CORE_CONTROLLERS:-empty}" = "refuse" ]; then
        emit '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":1003,"message":"system_profiler failed (fake core)"}}'
        continue
      fi
      emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"macos_version":"26.0","arch":"arm64","rosetta_present":true,"free_disk_bytes":123456789,"screen_recording":"unknown","developer_id_identities":0,"controllers":'"$(controllers_json)"'}}'
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
