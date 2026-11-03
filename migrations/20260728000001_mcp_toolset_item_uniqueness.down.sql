DROP INDEX idx_mcp_toolset_items_unique;

ALTER TABLE mcp_toolset_items
    ADD CONSTRAINT mcp_toolset_items_toolset_id_server_id_tool_name_key
    UNIQUE (toolset_id, server_id, tool_name);
