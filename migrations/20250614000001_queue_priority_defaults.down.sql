ALTER TABLE api_keys
    DROP COLUMN IF EXISTS default_priority;

ALTER TABLE teams
    DROP COLUMN IF EXISTS default_priority;
