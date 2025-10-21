-- Drop all tables in reverse order

DROP TRIGGER IF EXISTS update_users_updated_at ON users;
DROP TRIGGER IF EXISTS update_teams_updated_at ON teams;
DROP FUNCTION IF EXISTS update_updated_at_column();

DROP TABLE IF EXISTS request_logs CASCADE;
DROP TABLE IF EXISTS usage_logs CASCADE;
DROP TABLE IF EXISTS casbin_rule CASCADE;
DROP TABLE IF EXISTS api_keys CASCADE;
DROP TABLE IF EXISTS team_memberships CASCADE;
DROP TABLE IF EXISTS users CASCADE;
DROP TABLE IF EXISTS teams CASCADE;

DROP EXTENSION IF EXISTS "uuid-ossp";
