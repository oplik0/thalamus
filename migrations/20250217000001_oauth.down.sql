-- Revert OAuth support

-- Drop triggers
DROP TRIGGER IF EXISTS update_oauth_identities_updated_at ON oauth_identities;
DROP TRIGGER IF EXISTS update_oauth_providers_updated_at ON oauth_providers;

-- Drop indexes
DROP INDEX IF EXISTS idx_users_oauth_provider;
DROP INDEX IF EXISTS idx_oauth_identities_provider_user_id;
DROP INDEX IF EXISTS idx_oauth_identities_provider_id;
DROP INDEX IF EXISTS idx_oauth_identities_user_id;
DROP INDEX IF EXISTS idx_oauth_providers_active;
DROP INDEX IF EXISTS idx_oauth_providers_name;

-- Drop columns from users table
ALTER TABLE users DROP COLUMN IF EXISTS oauth_identity_id;
ALTER TABLE users DROP COLUMN IF EXISTS oauth_provider_id;

-- Drop tables
DROP TABLE IF EXISTS oauth_identities;
DROP TABLE IF EXISTS oauth_providers;
