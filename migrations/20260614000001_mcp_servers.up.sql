-- MCP servers and toolsets
-- Provides persistence for MCP gateway servers and named toolsets.

CREATE TABLE mcp_servers (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    alias TEXT NOT NULL,
    name TEXT,
    transport TEXT NOT NULL,
    url TEXT,
    command TEXT,
    args JSONB DEFAULT '[]'::jsonb,
    env JSONB DEFAULT '{}'::jsonb,
    auth JSONB DEFAULT '{"type": "none"}'::jsonb,
    static_headers JSONB DEFAULT '{}'::jsonb,
    extra_headers JSONB DEFAULT '[]'::jsonb,
    timeout TEXT DEFAULT '30s',
    description TEXT,
    allowed_scopes JSONB DEFAULT '[]'::jsonb,
    team_id UUID REFERENCES teams(id) ON DELETE SET NULL,
    created_by UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at TIMESTAMPTZ
);

-- Partial unique index so soft-deleted aliases can be reused.
CREATE UNIQUE INDEX idx_mcp_servers_alias_unique ON mcp_servers(alias) WHERE deleted_at IS NULL;
CREATE INDEX idx_mcp_servers_team ON mcp_servers(team_id) WHERE deleted_at IS NULL;

CREATE TRIGGER update_mcp_servers_updated_at
    BEFORE UPDATE ON mcp_servers
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Toolsets: named collections of tools from one or more MCP servers.
CREATE TABLE mcp_toolsets (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    team_id UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    description TEXT,
    is_auto BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at TIMESTAMPTZ
);

-- Partial unique index so soft-deleted (team_id, slug) pairs can be reused.
CREATE UNIQUE INDEX idx_mcp_toolsets_team_slug_unique ON mcp_toolsets(team_id, slug) WHERE deleted_at IS NULL;
CREATE INDEX idx_mcp_toolsets_team ON mcp_toolsets(team_id) WHERE deleted_at IS NULL;

CREATE TRIGGER update_mcp_toolsets_updated_at
    BEFORE UPDATE ON mcp_toolsets
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Items within a toolset. tool_name is NULL when the whole server is included.
CREATE TABLE mcp_toolset_items (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    toolset_id UUID NOT NULL REFERENCES mcp_toolsets(id) ON DELETE CASCADE,
    server_id UUID NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    tool_name TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (toolset_id, server_id, tool_name)
);

CREATE INDEX idx_mcp_toolset_items_toolset ON mcp_toolset_items(toolset_id);
CREATE INDEX idx_mcp_toolset_items_server ON mcp_toolset_items(server_id);

-- Ensure every team has an automatic MCP toolset at /toolsets/teams/<slug>/mcp.
CREATE OR REPLACE FUNCTION ensure_team_mcp_toolset()
RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO mcp_toolsets (team_id, name, slug, description, is_auto)
    VALUES (
        NEW.id,
        COALESCE(NEW.name || ' MCP Tools', 'MCP Tools'),
        'mcp',
        'Automatically created toolset for team ' || COALESCE(NEW.name, NEW.id::text),
        TRUE
    )
    ON CONFLICT (team_id, slug) WHERE deleted_at IS NULL DO NOTHING;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER team_mcp_toolset_auto_create
    AFTER INSERT ON teams
    FOR EACH ROW
    EXECUTE FUNCTION ensure_team_mcp_toolset();

-- Ensure existing teams have a slug for toolset routing.
UPDATE teams
SET slug = COALESCE(slug, REGEXP_REPLACE(LOWER(name), '[^a-z0-9]+', '-', 'g'))
WHERE slug IS NULL;

-- Backfill auto toolsets for existing teams.
INSERT INTO mcp_toolsets (team_id, name, slug, description, is_auto)
SELECT
    id,
    COALESCE(name || ' MCP Tools', 'MCP Tools'),
    'mcp',
    'Automatically created toolset for team ' || COALESCE(name, id::text),
    TRUE
FROM teams
WHERE deleted_at IS NULL
ON CONFLICT (team_id, slug) WHERE deleted_at IS NULL DO NOTHING;
