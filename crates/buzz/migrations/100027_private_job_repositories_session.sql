-- MAXPLAYER: session jobs (docs/specs/session-jobs.md §3.3). A repository whose
-- awarded claim advertised ["session","1"] stays open after a turn's ACCEPT; it
-- closes on REJECT (3407) or a FOLLOW-UP (3412) with ["status","close"]. Set once
-- at award binding. Existing rows are one-shot jobs.
ALTER TABLE private_job_repositories ADD COLUMN session BOOLEAN NOT NULL DEFAULT FALSE;
