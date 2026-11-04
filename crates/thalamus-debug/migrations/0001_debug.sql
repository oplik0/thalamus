-- Apply after the identity migrations. Debug records never contain model, forge,
-- or tool credential plaintext. The application authenticates principals before
-- supplying an identity and rechecks membership under the identity-user lock.
CREATE TABLE IF NOT EXISTS thalamus_debug_sessions (
    id UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'::uuid),
    user_id UUID NOT NULL REFERENCES identity_users(id),
    team_id UUID NOT NULL REFERENCES identity_teams(id),
    project_id UUID NOT NULL,
    client_request_key TEXT NOT NULL CHECK (octet_length(client_request_key) BETWEEN 1 AND 256 AND client_request_key ~ '^[!-~]+$'),
    service_id TEXT NOT NULL CHECK (octet_length(service_id) BETWEEN 1 AND 256 AND service_id ~ '^[!-~]+$'),
    run_id UUID CHECK (run_id IS NULL OR run_id <> '00000000-0000-0000-0000-000000000000'::uuid),
    model TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 256 AND model ~ '^[!-~]+$'),
    checkout_revision TEXT NOT NULL CHECK (checkout_revision ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    job_namespace TEXT NOT NULL CHECK (octet_length(job_namespace) BETWEEN 1 AND 63 AND job_namespace ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
    job_name TEXT NOT NULL CHECK (job_name = 'thalamus-debug-' || id::text),
    deadline TIMESTAMPTZ NOT NULL,
    model_budget BIGINT NOT NULL CHECK (model_budget > 0),
    tool_budget BIGINT NOT NULL CHECK (tool_budget > 0),
    tool_calls_used BIGINT NOT NULL DEFAULT 0 CHECK (tool_calls_used >= 0 AND tool_calls_used <= tool_budget),
    service_context JSONB NOT NULL CHECK (jsonb_typeof(service_context) = 'object' AND service_context <> '{}'::jsonb AND octet_length(service_context::text) <= 1048576),
    failing_evidence JSONB NOT NULL CHECK (jsonb_typeof(failing_evidence) = 'object' AND failing_evidence <> '{}'::jsonb AND octet_length(failing_evidence::text) <= 1048576),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'running', 'completed', 'failed', 'cancelled', 'timed_out')),
    last_transition_from TEXT CHECK (last_transition_from IN ('pending', 'running')),
    outcome JSONB CHECK (outcome IS NULL OR (jsonb_typeof(outcome) = 'object' AND outcome <> '{}'::jsonb AND octet_length(outcome::text) <= 1048576)),
    cleanup_required BOOLEAN NOT NULL DEFAULT FALSE,
    initialization_claim UUID CHECK (initialization_claim IS NULL OR initialization_claim <> '00000000-0000-0000-0000-000000000000'::uuid),
    upstream_session_id TEXT CHECK (upstream_session_id IS NULL OR (octet_length(upstream_session_id) BETWEEN 1 AND 128 AND upstream_session_id ~ '^[A-Za-z0-9_-]+$')),
    next_event_sequence BIGINT NOT NULL DEFAULT 1 CHECK (next_event_sequence > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, team_id, client_request_key),
    UNIQUE (id, user_id, team_id),
    FOREIGN KEY (project_id, team_id) REFERENCES identity_projects(id, team_id),
    CHECK (deadline > created_at),
    CHECK ((state IN ('pending', 'running') AND outcome IS NULL) OR
           (state IN ('completed', 'failed', 'cancelled', 'timed_out') AND outcome IS NOT NULL)),
    CHECK (NOT cleanup_required OR state IN ('completed', 'failed', 'cancelled', 'timed_out')),
    CHECK (upstream_session_id IS NULL OR initialization_claim IS NOT NULL)
);

CREATE TABLE IF NOT EXISTS thalamus_debug_events (
    session_id UUID NOT NULL REFERENCES thalamus_debug_sessions(id),
    sequence BIGINT NOT NULL CHECK (sequence > 0),
    client_request_key TEXT NOT NULL CHECK (octet_length(client_request_key) BETWEEN 1 AND 256 AND client_request_key ~ '^[!-~]+$'),
    kind TEXT NOT NULL CHECK (octet_length(kind) BETWEEN 1 AND 80 AND kind ~ '^[a-z0-9._-]+$'),
    payload JSONB NOT NULL CHECK (jsonb_typeof(payload) = 'object' AND payload <> '{}'::jsonb AND octet_length(payload::text) <= 1048576),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, sequence),
    UNIQUE (session_id, client_request_key)
);

CREATE TABLE IF NOT EXISTS thalamus_debug_tool_credentials (
    id UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'::uuid),
    session_id UUID NOT NULL,
    user_id UUID NOT NULL,
    team_id UUID NOT NULL,
    digest BYTEA NOT NULL UNIQUE CHECK (octet_length(digest) = 32),
    audience TEXT NOT NULL CHECK (octet_length(audience) BETWEEN 1 AND 256 AND audience ~ '^[!-~]+$'),
    expires_at TIMESTAMPTZ NOT NULL,
    issued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at TIMESTAMPTZ,
    FOREIGN KEY (session_id, user_id, team_id) REFERENCES thalamus_debug_sessions(id, user_id, team_id),
    CHECK (expires_at > issued_at)
);
CREATE UNIQUE INDEX IF NOT EXISTS thalamus_debug_one_live_tool_credential
    ON thalamus_debug_tool_credentials(session_id) WHERE revoked_at IS NULL;
CREATE INDEX IF NOT EXISTS thalamus_debug_sessions_owner
    ON thalamus_debug_sessions(user_id, team_id, created_at, id);
CREATE INDEX IF NOT EXISTS thalamus_debug_sessions_actionable
    ON thalamus_debug_sessions(created_at, id)
    WHERE state IN ('pending', 'running') OR cleanup_required;
