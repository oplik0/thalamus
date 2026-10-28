CREATE INDEX IF NOT EXISTS thalamus_runs_actionable ON thalamus_runs (created_at, id)
    WHERE state IN ('pending', 'running', 'ingesting');
