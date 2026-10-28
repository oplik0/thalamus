-- Apply after identity migrations. Labels reflect the source at observation time;
-- do not join projects to their current team when reading historical usage.
CREATE TABLE usage_daily (
    day DATE NOT NULL,
    team_id UUID NOT NULL,
    project_id UUID NOT NULL,
    service TEXT NOT NULL CHECK (length(service) BETWEEN 1 AND 256),
    traffic_class TEXT NOT NULL CHECK (length(traffic_class) BETWEEN 1 AND 256),
    input_tokens DOUBLE PRECISION CHECK (input_tokens >= 0),
    output_tokens DOUBLE PRECISION CHECK (output_tokens >= 0),
    observed_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (day, team_id, project_id, service, traffic_class)
);
CREATE INDEX usage_daily_team_day_idx ON usage_daily(team_id, day);

-- Worker-only source observations, including days with no returned series.
-- Completeness is relative to returned series, not proof of global coverage.
CREATE TABLE usage_ingestion_days (
    day DATE PRIMARY KEY,
    observed_at TIMESTAMPTZ NOT NULL,
    complete BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX usage_ingestion_incomplete_idx ON usage_ingestion_days(observed_at, day) WHERE NOT complete;
