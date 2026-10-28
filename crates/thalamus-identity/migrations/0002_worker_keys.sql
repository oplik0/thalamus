ALTER TABLE identity_projects ADD COLUMN budget DOUBLE PRECISION CHECK (budget IS NULL OR budget >= 0);
ALTER TABLE identity_keys ADD COLUMN name TEXT NOT NULL DEFAULT 'key' CHECK (length(trim(name)) > 0);
ALTER TABLE identity_keys ALTER COLUMN expires_at DROP NOT NULL;
ALTER TABLE identity_keys ADD COLUMN run_id UUID UNIQUE;
ALTER TABLE identity_keys ADD CONSTRAINT identity_run_key_scope CHECK (run_id IS NULL OR (kind = 'job' AND project_id IS NOT NULL AND expires_at IS NOT NULL AND cardinality(allowed_services) = 1));
