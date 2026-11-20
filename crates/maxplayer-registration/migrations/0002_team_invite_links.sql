-- Team invite links (docs/specs/maxplayer-team.md "Invite links"). A link is
-- https://<site>/join/<token>; only SHA-256(token) is stored. The token is 32 random bytes, so
-- no pepper is needed. Any linked member of the team creates one; redeeming adds the joiner
-- with a kind 9000 signed by the service-wide invite key.
CREATE TABLE team_invite_links (
    id          BIGSERIAL PRIMARY KEY,
    token_hash  BYTEA NOT NULL UNIQUE,
    channel     TEXT NOT NULL CHECK (channel ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
    created_by  TEXT NOT NULL CHECK (created_by ~ '^[0-9a-f]{64}$'),
    created_at  TIMESTAMPTZ NOT NULL,
    expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at  TIMESTAMPTZ
);
CREATE INDEX team_invite_links_channel_idx ON team_invite_links (channel);

-- Teams the invite key may still be a member of. The sweep makes the key leave a team once it
-- has no live link, then deletes the row. Creating a link updates the row, so a sweep holding
-- its lock and a new link never interleave.
CREATE TABLE team_invite_channels (
    channel    TEXT PRIMARY KEY,
    updated_at TIMESTAMPTZ NOT NULL
);
