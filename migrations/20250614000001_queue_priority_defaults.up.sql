-- Add default queue priority columns for teams and API keys.
-- NULL means "fall back to the next level" (key -> team -> config default).

ALTER TABLE teams
    ADD COLUMN default_priority VARCHAR(50);

ALTER TABLE api_keys
    ADD COLUMN default_priority VARCHAR(50);
