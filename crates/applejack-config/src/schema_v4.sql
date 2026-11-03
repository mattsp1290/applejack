-- Schema version 4. The owned library is cached in `apps`: a row with
-- `fetched_at` came from Steam, a row with `last_installed_buildid` is whole
-- on disk, and a row with `library_dir` and no build id is an install that has
-- not finished. The launch exe and arguments are what the app info chose;
-- `game_config.launch_exe_override` still wins.

ALTER TABLE apps ADD COLUMN size_bytes INTEGER;
ALTER TABLE apps ADD COLUMN default_branch TEXT;
ALTER TABLE apps ADD COLUMN latest_buildid INTEGER;
ALTER TABLE apps ADD COLUMN launch_exe TEXT;
ALTER TABLE apps ADD COLUMN launch_args TEXT;
