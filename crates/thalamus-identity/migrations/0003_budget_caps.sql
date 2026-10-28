CREATE TABLE identity_budget_projection (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    source_commit TEXT NOT NULL,
    refreshed_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE identity_budget_caps (
    team_id UUID PRIMARY KEY REFERENCES identity_teams(id),
    max_tokens BIGINT NOT NULL CHECK (max_tokens >= 0),
    unit TEXT NOT NULL CHECK (unit = 'tokens'),
    budget_window TEXT NOT NULL CHECK (budget_window IN ('day', 'month'))
);
