-- Revert Teams and Projects migration

-- Drop usage_logs extensions
DROP INDEX IF EXISTS idx_usage_logs_project_id;
ALTER TABLE usage_logs DROP COLUMN IF EXISTS project_id;

-- Drop api_keys extensions
DROP INDEX IF EXISTS idx_api_keys_project_id;
ALTER TABLE api_keys DROP COLUMN IF EXISTS project_id;

-- Drop projects table
DROP TRIGGER IF EXISTS update_projects_updated_at ON projects;
DROP TABLE IF EXISTS projects CASCADE;

-- Revert team_memberships extensions
DROP INDEX IF EXISTS idx_team_memberships_deleted_at;
ALTER TABLE team_memberships DROP COLUMN IF EXISTS deleted_at;

-- Note: We cannot restore the exact CHECK constraint data if rows exist
-- with roles outside the original set, but we recreate the constraint for cleanliness.
ALTER TABLE team_memberships ADD CONSTRAINT team_memberships_role_check CHECK (role IN ('admin', 'member', 'readonly'));

-- Revert teams extensions
DROP INDEX IF EXISTS idx_teams_parent_team_id;
ALTER TABLE teams DROP COLUMN IF EXISTS slug;
ALTER TABLE teams DROP COLUMN IF EXISTS is_active;
ALTER TABLE teams DROP COLUMN IF EXISTS parent_team_id;
