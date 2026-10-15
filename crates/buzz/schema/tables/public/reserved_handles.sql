-- Reserved names also serve as the operator-managed abuse blocklist.
CREATE TABLE reserved_handles (
    community_id UUID NOT NULL REFERENCES communities(id),
    name TEXT NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9._-]{2,29}$'),
    PRIMARY KEY (community_id, name)
);
