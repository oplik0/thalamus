-- Teams and Projects migration
-- Extends teams, team_memberships, api_keys, usage_logs
-- Creates projects table

-- Extend teams table
ALTER TABLE teams
    ADD COLUMN parent_team_id UUID REFERENCES teams(id) ON DELETE SET NULL,
    ADD COLUMN is_active BOOLEAN DEFAULT TRUE,
    ADD COLUMN slug VARCHAR(255) UNIQUE;

CREATE INDEX idx_teams_parent_team_id ON teams(parent_team_id) WHERE parent_team_id IS NOT NULL;

-- Extend team_memberships
ALTER TABLE team_memberships
    ADD COLUMN deleted_at TIMESTAMPTZ;

ALTER TABLE team_memberships DROP CONSTRAINT IF EXISTS team_memberships_role_check;

CREATE INDEX idx_team_memberships_deleted_at ON team_memberships(deleted_at) WHERE deleted_at IS NOT NULL;

-- Create projects table
CREATE TABLE projects (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    team_id UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    description TEXT,
    metadata JSONB DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at TIMESTAMPTZ,
    CONSTRAINT unique_team_project_name UNIQUE(team_id, name) WHERE deleted_at IS NULL
);

-- Trigger for updated_at on projects
CREATE TRIGGER update_projects_updated_at
    BEFORE UPDATE ON projects
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Extend api_keys
ALTER TABLE api_keys
    ADD COLUMN project_id UUID REFERENCES projects(id) ON DELETE SET NULL;

CREATE INDEX idx_api_keys_project_id ON api_keys(project_id) WHERE project_id IS NOT NULL;

-- Extend usage_logs
ALTER TABLE usage_logs
    ADD COLUMN project_id UUID REFERENCES projects(id) ON DELETE SET NULL;

CREATE INDEX idx_usage_logs_project_id ON usage_logs(project_id) WHERE project_id IS NOT NULL;
