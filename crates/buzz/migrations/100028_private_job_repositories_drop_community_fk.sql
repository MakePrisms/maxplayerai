-- MAXPLAYER: upstream's 0101-0104 drop the relay's remaining foreign keys
-- (docs/database.md Rule 2); the community is checked in the writer's own
-- transaction. 100025 created this table with one. The deletion catalog purges
-- private_job_repositories before communities, so nothing relies on it.
SET LOCAL lock_timeout = '5s';
ALTER TABLE private_job_repositories DROP CONSTRAINT private_job_repositories_community_id_fkey;
