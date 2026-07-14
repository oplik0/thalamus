-- Add health, enable/disable, and tool-sync columns to MCP servers.

ALTER TABLE mcp_servers
    ADD COLUMN is_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN is_healthy BOOLEAN,
    ADD COLUMN last_health_check_at TIMESTAMPTZ,
    ADD COLUMN consecutive_health_failures INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN last_tool_sync_at TIMESTAMPTZ;

COMMENT ON COLUMN mcp_servers.is_enabled IS 'When false, the server is disabled and no connections are attempted';
COMMENT ON COLUMN mcp_servers.is_healthy IS 'NULL until first health check; then reflects current connectivity';
COMMENT ON COLUMN mcp_servers.consecutive_health_failures IS 'Incremented on each failed check, reset on success';
COMMENT ON COLUMN mcp_servers.last_tool_sync_at IS 'Last time tools/list was successfully fetched';
