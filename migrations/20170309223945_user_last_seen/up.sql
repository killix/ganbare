ALTER TABLE users ADD COLUMN last_seen TIMESTAMPTZ NOT NULL DEFAULT now();
