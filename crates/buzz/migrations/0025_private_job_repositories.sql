-- Private job repositories live in the buyer's opaque job namespace. Access decisions
-- always use the writer DB; a read replica is not an authorization oracle.
CREATE TABLE private_job_repositories (
    community_id UUID NOT NULL REFERENCES communities(id),
    buyer TEXT NOT NULL CHECK (buyer ~ '^[0-9a-f]{64}$'),
    job_id TEXT NOT NULL CHECK (job_id ~ '^[0-9a-f]{64}$'),
    offer_id TEXT NOT NULL CHECK (offer_id ~ '^[0-9a-f]{64}$'),
    target TEXT CHECK (target ~ '^[0-9a-f]{64}$'),
    seller TEXT CHECK (seller ~ '^[0-9a-f]{64}$'),
    award_id TEXT CHECK (award_id ~ '^[0-9a-f]{64}$'),
    service TEXT NOT NULL CHECK (service ~ '^[0-9a-f]{64}$'),
    closed BOOLEAN NOT NULL DEFAULT FALSE,
    input_frozen BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (community_id,buyer,job_id),
    UNIQUE (community_id,offer_id),
    CHECK ((seller IS NULL) = (award_id IS NULL)),
    CHECK (seller IS NULL OR target IS NULL OR target = seller)
);
