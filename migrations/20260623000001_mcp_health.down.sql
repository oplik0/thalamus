ALTER TABLE mcp_servers
    DROP COLUMN IF EXISTS is_enabled,
    DROP COLUMN IF EXISTS is_healthy,
    DROP COLUMN IF EXISTS last_health_check_at,
    DROP COLUMN IF EXISTS consecutive_health_failures,
    DROP COLUMN IF EXISTS last_tool_sync_at;
