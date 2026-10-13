-- MAXPLAYER: attach the community write fence (migration 0029) to
-- private_job_repositories. Where the table predates 0029 (a relay upgraded
-- from the pre-merge fork), 0029's bootstrap loop already attached it and this
-- is a no-op. On a fresh database the table is created after 0029 and needs
-- this explicit call.
SELECT attach_community_write_fence('private_job_repositories');
