CREATE TABLE identity_users (
    id UUID PRIMARY KEY,
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    disabled BOOLEAN NOT NULL DEFAULT FALSE,
    platform_admin BOOLEAN NOT NULL DEFAULT FALSE,
    -- Equal issuance times are ambiguous and clear privileges.
    groups_issued_at TIMESTAMPTZ,
    UNIQUE (issuer, subject)
);
CREATE TABLE identity_teams (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE CHECK (length(name) > 0)
);
CREATE TABLE identity_memberships (
    user_id UUID NOT NULL REFERENCES identity_users(id),
    team_id UUID NOT NULL REFERENCES identity_teams(id),
    role TEXT NOT NULL CHECK (role IN ('team_admin', 'member')),
    PRIMARY KEY (user_id, team_id)
);
CREATE TABLE identity_projects (
    id UUID PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES identity_teams(id),
    name TEXT NOT NULL CHECK (length(name) > 0),
    cost_centre TEXT,
    UNIQUE (team_id, name),
    UNIQUE (id, team_id)
);
CREATE TABLE identity_login_attempts (
    state TEXT PRIMARY KEY,
    nonce TEXT NOT NULL,
    pkce_verifier TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE identity_sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES identity_users(id),
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE identity_keys (
    id UUID PRIMARY KEY,
    digest BYTEA NOT NULL UNIQUE CHECK (octet_length(digest) = 32),
    user_id UUID NOT NULL REFERENCES identity_users(id),
    creator_id UUID NOT NULL REFERENCES identity_users(id),
    team_id UUID NOT NULL REFERENCES identity_teams(id),
    project_id UUID,
    kind TEXT NOT NULL CHECK (kind IN ('personal', 'project', 'job')),
    allowed_services TEXT[] NOT NULL,
    budget BIGINT CHECK (budget IS NULL OR budget >= 0),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (project_id, team_id) REFERENCES identity_projects(id, team_id),
    CHECK ((kind = 'personal') OR project_id IS NOT NULL)
);
CREATE INDEX identity_keys_team_idx ON identity_keys(team_id);
CREATE INDEX identity_keys_owner_idx ON identity_keys(user_id);
CREATE TABLE identity_audit (
    id BIGSERIAL PRIMARY KEY,
    actor_id UUID REFERENCES identity_users(id),
    agent TEXT,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    details JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
