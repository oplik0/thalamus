-- Revert MCP servers and toolsets
--
-- Note: The up migration backfills NULL team slugs with derived values.
-- Those slug values are intentionally left in place on revert because
-- reverting them to NULL would break team routing for existing teams.

DROP TRIGGER IF EXISTS team_mcp_toolset_auto_create ON teams;
DROP FUNCTION IF EXISTS ensure_team_mcp_toolset();

DROP TABLE IF EXISTS mcp_toolset_items;
DROP TABLE IF EXISTS mcp_toolsets;
DROP TABLE IF EXISTS mcp_servers;
