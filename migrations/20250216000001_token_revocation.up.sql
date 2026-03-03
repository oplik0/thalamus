-- Token revocation and refresh token support
-- Enables secure token lifecycle management

-- Token revocation list (for explicit logout/blacklist)
-- Stores revoked token JTIs with optional expiration for automatic cleanup
CREATE TABLE token_revocations (
    token_jti UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    revoked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL, -- When the original token expires (for cleanup)
    reason VARCHAR(50) DEFAULT 'logout', -- 'logout', 'revoked', 'compromised'
    revoked_by UUID REFERENCES users(id) ON DELETE SET NULL -- Admin who revoked (if applicable)
);

-- Refresh tokens table (for refresh token rotation pattern)
CREATE TABLE refresh_tokens (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    team_id UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,

    -- Token hash (SHA-256 of the token itself, not the full key)
    token_hash VARCHAR(64) NOT NULL UNIQUE,

    -- Token family for rotation detection
    family UUID NOT NULL, -- All tokens in a family share this ID
    parent_token_jti UUID, -- Previous token in the chain (for detecting reuse)

    -- Scopes and roles (copied from original authentication)
    scopes TEXT[],
    roles TEXT[],

    -- Status
    is_active BOOLEAN DEFAULT TRUE,
    revoked_at TIMESTAMPTZ,
    revoked_reason VARCHAR(50),

    -- Expiration
    expires_at TIMESTAMPTZ NOT NULL,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at TIMESTAMPTZ,

    -- Constraint: parent_token_jti must reference a valid token if set
    CONSTRAINT valid_parent CHECK (parent_token_jti IS NULL OR parent_token_jti != id)
);

-- API Key rotation support
-- Add rotation tracking to api_keys table
ALTER TABLE api_keys
    ADD COLUMN IF NOT EXISTS rotated_from UUID REFERENCES api_keys(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS rotated_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS grace_period_ends_at TIMESTAMPTZ, -- Old key valid until this time
    ADD COLUMN IF NOT EXISTS rotation_reason VARCHAR(50);

-- Signing keys table for HTTP Signatures (RFC 9421)
CREATE TABLE signing_keys (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    key_id VARCHAR(255) NOT NULL UNIQUE, -- User-facing key identifier (e.g., "key_abc123")

    -- Ownership
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    team_id UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,

    -- Key material (public key only - private key is one-time generated and returned)
    public_key TEXT NOT NULL,
    algorithm VARCHAR(50) NOT NULL CHECK (algorithm IN ('ed25519', 'rsa-pss-sha512', 'ecdsa-p256-sha256')),
    key_fingerprint VARCHAR(64) NOT NULL, -- SHA-256 fingerprint for quick lookup

    -- Metadata
    name VARCHAR(255),
    description TEXT,

    -- Scopes (what this key can be used for)
    scopes TEXT[],

    -- Status
    is_active BOOLEAN DEFAULT TRUE,
    revoked_at TIMESTAMPTZ,
    revoked_reason VARCHAR(50),

    -- Expiration
    expires_at TIMESTAMPTZ,

    -- Usage tracking
    last_used_at TIMESTAMPTZ,
    use_count INTEGER DEFAULT 0,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for performance
CREATE INDEX idx_token_revocations_user_id ON token_revocations(user_id);
CREATE INDEX idx_token_revocations_expires_at ON token_revocations(expires_at);
CREATE INDEX idx_refresh_tokens_user_id ON refresh_tokens(user_id);
CREATE INDEX idx_refresh_tokens_family ON refresh_tokens(family);
CREATE INDEX idx_refresh_tokens_token_hash ON refresh_tokens(token_hash);
CREATE INDEX idx_refresh_tokens_expires_at ON refresh_tokens(expires_at) WHERE is_active = TRUE;
CREATE INDEX idx_api_keys_rotated_from ON api_keys(rotated_from) WHERE rotated_from IS NOT NULL;
CREATE INDEX idx_signing_keys_key_id ON signing_keys(key_id);
CREATE INDEX idx_signing_keys_user_id ON signing_keys(user_id);
CREATE INDEX idx_signing_keys_team_id ON signing_keys(team_id);
CREATE INDEX idx_signing_keys_fingerprint ON signing_keys(key_fingerprint);
CREATE INDEX idx_signing_keys_active ON signing_keys(is_active) WHERE is_active = TRUE;

-- Comment on tables
COMMENT ON TABLE token_revocations IS 'Stores revoked PASETO token JTIs for explicit revocation';
COMMENT ON TABLE refresh_tokens IS 'Refresh tokens for short-lived access token pattern with rotation detection';
COMMENT ON TABLE signing_keys IS 'Public keys for HTTP Message Signatures (RFC 9421)';
