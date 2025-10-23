-- Initial database schema for Thalamus

-- Enable UUID extension
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

-- Teams table
CREATE TABLE teams (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    name VARCHAR(255) NOT NULL UNIQUE,
    description TEXT,

    -- Budget and limits
    budget_limit_usd DECIMAL(12, 2),
    rate_limit_rpm INTEGER DEFAULT 60,
    rate_limit_burst INTEGER DEFAULT 10,

    -- Access control
    allowed_models TEXT[], -- Array of model names, NULL means all allowed
    allowed_backends TEXT[], -- Array of backend names, NULL means all allowed
    allowed_tags TEXT[], -- Array of tags for backend filtering

    -- Logging policy
    logging_policy VARCHAR(50) DEFAULT 'metadata' CHECK (logging_policy IN ('full', 'metadata', 'zero', 'compliance')),
    log_retention_days INTEGER DEFAULT 30,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at TIMESTAMPTZ
);

-- Users table
CREATE TABLE users (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    username VARCHAR(255) NOT NULL UNIQUE,
    email VARCHAR(255) NOT NULL UNIQUE,

    -- User type
    is_service_account BOOLEAN DEFAULT FALSE,

    -- Status
    is_active BOOLEAN DEFAULT TRUE,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login_at TIMESTAMPTZ
);

-- Team memberships (many-to-many relationship)
CREATE TABLE team_memberships (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    team_id UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,

    -- Role within the team (for Casbin)
    role VARCHAR(50) NOT NULL DEFAULT 'member' CHECK (role IN ('admin', 'member', 'readonly')),

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Ensure unique user-team combination
    UNIQUE(user_id, team_id)
);

-- API Keys table
CREATE TABLE api_keys (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    key_id VARCHAR(255) NOT NULL UNIQUE, -- Prefix + random string (e.g., thl_abc123)
    key_hash TEXT NOT NULL, -- Argon2 hash of the full key
    key_prefix VARCHAR(20) NOT NULL, -- First 8 chars for display (e.g., thl_abc1)

    -- Ownership
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    team_id UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,

    -- Metadata
    name VARCHAR(255) NOT NULL, -- User-friendly name for the key
    description TEXT,

    -- Permissions
    scopes TEXT[], -- Array of permission scopes (e.g., 'chat:read', 'chat:write')

    -- Status
    is_active BOOLEAN DEFAULT TRUE,
    last_used_at TIMESTAMPTZ,

    -- Expiration
    expires_at TIMESTAMPTZ,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at TIMESTAMPTZ
);

-- Usage logs table (for cost tracking and analytics)
CREATE TABLE usage_logs (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),

    -- Request identification
    request_id UUID NOT NULL,

    -- Who made the request
    user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    team_id UUID REFERENCES teams(id) ON DELETE SET NULL,
    api_key_id UUID REFERENCES api_keys(id) ON DELETE SET NULL,

    -- Request details
    model VARCHAR(255) NOT NULL,
    backend VARCHAR(255) NOT NULL,
    endpoint VARCHAR(255) NOT NULL,

    -- Token usage
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    total_tokens INTEGER,

    -- Cost (in USD)
    cost_usd DECIMAL(10, 6),

    -- Performance
    latency_ms INTEGER,
    queue_time_ms INTEGER,

    -- Status
    status_code INTEGER,
    error_message TEXT,

    -- Timestamp
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Request logs table (detailed request/response logging based on team policy)
CREATE TABLE request_logs (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),

    -- Links to usage log
    request_id UUID NOT NULL,
    usage_log_id UUID REFERENCES usage_logs(id) ON DELETE CASCADE,

    -- Full request/response (only stored based on team logging policy)
    request_body JSONB,
    response_body JSONB,
    request_headers JSONB,
    response_headers JSONB,

    -- Metadata
    ip_address INET,
    user_agent TEXT,

    -- Timestamp
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Casbin policy table (for authorization)
CREATE TABLE casbin_rule (
    id SERIAL PRIMARY KEY,
    ptype VARCHAR(12) NOT NULL,
    v0 VARCHAR(128) NOT NULL DEFAULT '',
    v1 VARCHAR(128) NOT NULL DEFAULT '',
    v2 VARCHAR(128) NOT NULL DEFAULT '',
    v3 VARCHAR(128) NOT NULL DEFAULT '',
    v4 VARCHAR(128) NOT NULL DEFAULT '',
    v5 VARCHAR(128) NOT NULL DEFAULT '',
    CONSTRAINT unique_key_sqlx_adapter UNIQUE(ptype, v0, v1, v2, v3, v4, v5)
);

-- Indexes for performance
CREATE INDEX idx_api_keys_user_id ON api_keys(user_id);
CREATE INDEX idx_api_keys_team_id ON api_keys(team_id);
CREATE INDEX idx_api_keys_key_id ON api_keys(key_id);
CREATE INDEX idx_api_keys_active ON api_keys(is_active) WHERE is_active = TRUE;

CREATE INDEX idx_team_memberships_user_id ON team_memberships(user_id);
CREATE INDEX idx_team_memberships_team_id ON team_memberships(team_id);

CREATE INDEX idx_usage_logs_user_id ON usage_logs(user_id);
CREATE INDEX idx_usage_logs_team_id ON usage_logs(team_id);
CREATE INDEX idx_usage_logs_created_at ON usage_logs(created_at DESC);
CREATE INDEX idx_usage_logs_request_id ON usage_logs(request_id);

CREATE INDEX idx_request_logs_request_id ON request_logs(request_id);
CREATE INDEX idx_request_logs_usage_log_id ON request_logs(usage_log_id);

CREATE INDEX idx_casbin_rule_ptype ON casbin_rule(ptype);
CREATE INDEX idx_casbin_rule_v0_v1 ON casbin_rule(v0, v1);

-- Updated_at trigger function
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ language 'plpgsql';

-- Apply updated_at trigger to teams
CREATE TRIGGER update_teams_updated_at
    BEFORE UPDATE ON teams
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Apply updated_at trigger to users
CREATE TRIGGER update_users_updated_at
    BEFORE UPDATE ON users
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Insert default team and admin user (for development)
INSERT INTO teams (name, description, rate_limit_rpm, rate_limit_burst, logging_policy)
VALUES ('default', 'Default team for development', 1000, 50, 'metadata');

INSERT INTO users (username, email, is_service_account, is_active)
VALUES ('admin', 'admin@thalamus.local', false, true);

-- Add admin to default team as admin
INSERT INTO team_memberships (user_id, team_id, role)
SELECT u.id, t.id, 'admin'
FROM users u, teams t
WHERE u.username = 'admin' AND t.name = 'default';

-- Insert default Casbin policies
-- Admin role can do everything
INSERT INTO casbin_rule (ptype, v0, v1, v2, v3, v4, v5)
VALUES
    ('p', 'admin', '*', '*', '*', '', ''),
    ('p', 'member', '*', '/v1/chat/completions', 'POST', '', ''),
    ('p', 'member', '*', '/v1/batch/chat/completions', 'POST', '', ''),
    ('p', 'member', '*', '/v1/models', 'GET', '', ''),
    ('p', 'member', '*', '/health', 'GET', '', ''),
    ('p', 'readonly', '*', '/v1/models', 'GET', '', ''),
    ('p', 'readonly', '*', '/health', 'GET', '', '');

-- Assign admin role to admin user in default team
INSERT INTO casbin_rule (ptype, v0, v1, v2, v3, v4, v5)
SELECT 'g', u.username, 'admin', t.name, '', '', ''
FROM users u, teams t
WHERE u.username = 'admin' AND t.name = 'default';
