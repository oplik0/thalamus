-- Revert token revocation and rotation support

DROP TABLE IF EXISTS signing_keys;
DROP TABLE IF EXISTS refresh_tokens;
DROP TABLE IF EXISTS token_revocations;

ALTER TABLE api_keys
    DROP COLUMN IF EXISTS rotated_from,
    DROP COLUMN IF EXISTS rotated_at,
    DROP COLUMN IF EXISTS grace_period_ends_at,
    DROP COLUMN IF EXISTS rotation_reason;
