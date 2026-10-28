-- Existing requests cannot be assigned a project safely without operator input.
-- NOT VALID leaves those historical rows untouched while rejecting new invalid rows
-- and any attempt to update a legacy row before it has been migrated.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid = 'thalamus_runs'::regclass AND conname = 'thalamus_runs_project_id_required') THEN
        ALTER TABLE thalamus_runs ADD CONSTRAINT thalamus_runs_project_id_required CHECK (
            COALESCE(jsonb_typeof(request->'project_id') = 'string'
                AND request->>'project_id' ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                AND request->>'project_id' <> '00000000-0000-0000-0000-000000000000', false)
        ) NOT VALID;
    END IF;
END $$;
