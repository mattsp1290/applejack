-- Schema version 2. engine_versions.active is the only record of the active
-- version, so at most one row per engine may carry it.

CREATE UNIQUE INDEX engine_versions_one_active
    ON engine_versions (engine_id) WHERE active = 1;
