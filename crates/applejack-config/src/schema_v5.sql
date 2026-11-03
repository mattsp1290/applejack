-- Schema version 5. The branch a title was installed from: `--update` keeps
-- it, and `latest_buildid` describes `default_branch` only.

ALTER TABLE apps ADD COLUMN installed_branch TEXT;
