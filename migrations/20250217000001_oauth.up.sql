-- OAuth support for Thalamus

-- OAuth providers configuration table
CREATE TABLE oauth_providers (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    name VARCHAR(255) NOT NULL UNIQUE,
    provider_type VARCHAR(50) NOT NULL CHECK (provider_type IN ('github', 'github_enterprise', 'oidc')),
    client_id_encrypted TEXT NOT NULL,
    client_secret_encrypted TEXT NOT NULL,
    config_json JSONB DEFAULT '{}',
    is_active BOOLEAN DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- OAuth identities (links users to OAuth accounts)
CREATE TABLE oauth_identities (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider_id UUID NOT NULL REFERENCES oauth_providers(id) ON DELETE CASCADE,
    provider_user_id VARCHAR(255) NOT NULL,
    email VARCHAR(255),
    username VARCHAR(255),
    access_token_encrypted TEXT,
    refresh_token_encrypted TEXT,
    token_expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(provider_id, provider_user_id)
);

-- Add OAuth tracking to users table
ALTER TABLE users ADD COLUMN oauth_provider_id UUID REFERENCES oauth_providers(id) ON DELETE SET NULL;
ALTER TABLE users ADD COLUMN oauth_identity_id UUID REFERENCES oauth_identities(id) ON DELETE SET NULL;

-- Indexes for performance
CREATE INDEX idx_oauth_providers_name ON oauth_providers(name);
CREATE INDEX idx_oauth_providers_active ON oauth_providers(is_active) WHERE is_active = TRUE;
CREATE INDEX idx_oauth_identities_user_id ON oauth_identities(user_id);
CREATE INDEX idx_oauth_identities_provider_id ON oauth_identities(provider_id);
CREATE INDEX idx_oauth_identities_provider_user_id ON oauth_identities(provider_user_id);
CREATE INDEX idx_users_oauth_provider ON users(oauth_provider_id);

-- Updated_at trigger for oauth_providers
CREATE TRIGGER update_oauth_providers_updated_at
    BEFORE UPDATE ON oauth_providers
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Updated_at trigger for oauth_identities
CREATE TRIGGER update_oauth_identities_updated_at
    BEFORE UPDATE ON oauth_identities
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();
