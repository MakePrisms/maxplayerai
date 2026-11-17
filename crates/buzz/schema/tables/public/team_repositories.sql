-- MAXPLAYER: team repositories, created by migration 100028.
-- Keep desired-state schema parity with the embedded SQLx migration path.
CREATE TABLE team_repositories (
    community_id UUID NOT NULL REFERENCES communities(id),
    owner TEXT NOT NULL CHECK (owner ~ '^[0-9a-f]{64}$'),
    team_id TEXT NOT NULL CHECK (team_id ~ '^[0-9a-f]{64}$'),
    members TEXT[] NOT NULL,
    service TEXT NOT NULL CHECK (service ~ '^[0-9a-f]{64}$'),
    version BIGINT NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (community_id,owner,team_id),
    CHECK (owner = ANY(members))
);
