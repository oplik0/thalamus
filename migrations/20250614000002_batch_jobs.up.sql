-- Persistent batch jobs for OpenAI-style /v1/batch/chat/completions endpoint.
-- Jobs are queued at low priority and processed asynchronously by a worker.

CREATE TYPE batch_job_status AS ENUM ('pending', 'processing', 'completed', 'failed', 'cancelled');

CREATE TABLE batch_jobs (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),

    -- Ownership
    team_id UUID REFERENCES teams(id) ON DELETE SET NULL,
    user_id UUID REFERENCES users(id) ON DELETE SET NULL,

    -- Request payload
    request_body JSONB NOT NULL,

    -- Results / errors
    status batch_job_status NOT NULL DEFAULT 'pending',
    response_body JSONB,
    error_message TEXT,

    -- Metadata
    request_count INTEGER NOT NULL DEFAULT 0,
    completed_count INTEGER NOT NULL DEFAULT 0,
    failed_count INTEGER NOT NULL DEFAULT 0,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ
);

CREATE INDEX idx_batch_jobs_status_created_at ON batch_jobs(status, created_at);
CREATE INDEX idx_batch_jobs_team_id ON batch_jobs(team_id);
