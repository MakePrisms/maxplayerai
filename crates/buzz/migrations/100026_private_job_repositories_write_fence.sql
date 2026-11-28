-- MAXPLAYER: retired. This migration attached the community write fence
-- (0029) to private_job_repositories. Upstream's 0090 retired the fence and
-- dropped attach_community_write_fence, and on a fresh database 0090 runs
-- before this one, so it now does nothing. Databases that applied the original
-- keep their record: crates/buzz-db/src/runtime/migration.rs moves its stored
-- checksum to this file's and drops the trigger before 0090 runs.
SELECT 1;
