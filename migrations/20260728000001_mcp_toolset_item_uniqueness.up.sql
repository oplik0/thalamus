-- Treat whole-server toolset entries (tool_name IS NULL) as duplicates.

DELETE FROM mcp_toolset_items older
USING mcp_toolset_items newer
WHERE older.toolset_id = newer.toolset_id
  AND older.server_id = newer.server_id
  AND older.tool_name IS NOT DISTINCT FROM newer.tool_name
  AND older.id > newer.id;

ALTER TABLE mcp_toolset_items
    DROP CONSTRAINT mcp_toolset_items_toolset_id_server_id_tool_name_key;

CREATE UNIQUE INDEX idx_mcp_toolset_items_unique
    ON mcp_toolset_items (toolset_id, server_id, tool_name) NULLS NOT DISTINCT;
