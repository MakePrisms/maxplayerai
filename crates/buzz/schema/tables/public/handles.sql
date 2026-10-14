-- Explicit registry: kind-0 profile metadata never grants ownership.
CREATE TABLE handles (
    id BIGSERIAL PRIMARY KEY,
    community_id UUID NOT NULL REFERENCES communities(id),
    name TEXT NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9._-]{2,29}$'),
    pubkey BYTEA NOT NULL CHECK (octet_length(pubkey) = 32),
    status TEXT NOT NULL CHECK (status IN ('active', 'released')),
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    released_at TIMESTAMPTZ,
    CHECK ((status = 'active' AND released_at IS NULL) OR
           (status = 'released' AND released_at IS NOT NULL))
);
CREATE UNIQUE INDEX handles_active_name ON handles (community_id, name) WHERE status = 'active';
CREATE UNIQUE INDEX handles_active_pubkey ON handles (community_id, pubkey) WHERE status = 'active';
CREATE INDEX handles_cooldown ON handles (community_id, name, released_at DESC) WHERE status = 'released';
