-- Preserve the existing local observation time without presenting it as the
-- upstream forge's merge timestamp. The latter is independently supplied by
-- the forge and can remain unknown.
ALTER TABLE thalamus_rollouts RENAME COLUMN merge_completed_at TO merge_observed_at;
ALTER TABLE thalamus_rollouts ADD COLUMN upstream_merged_at TIMESTAMPTZ;
