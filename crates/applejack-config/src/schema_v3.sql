-- Schema version 3. A session row is written before the launch and closed by
-- whichever process owns it, so another process can see, cancel, and recover
-- it. At most one open row per title: this is what a double launch hits.

ALTER TABLE sessions ADD COLUMN session_key TEXT;
ALTER TABLE sessions ADD COLUMN prefix TEXT;
ALTER TABLE sessions ADD COLUMN launch_exe TEXT;
ALTER TABLE sessions ADD COLUMN state TEXT
    CHECK (state IN ('starting', 'running', 'exited', 'cancelled', 'failed', 'lost'));
ALTER TABLE sessions ADD COLUMN note TEXT;
ALTER TABLE sessions ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0;

-- Nothing wrote this table before now, but a row that did would have no state
-- and no way to close, and two open ones would fail the index below.
UPDATE sessions
SET state = 'lost',
    ended_at = COALESCE(ended_at, started_at, ''),
    note = 'recorded before sessions had states';

CREATE UNIQUE INDEX sessions_one_open ON sessions (appid) WHERE ended_at IS NULL;
