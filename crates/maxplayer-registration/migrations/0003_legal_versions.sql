-- Terms and privacy versions learned from the website's legal versions manifest
-- (`legal.versions_url`, src/legal.rs). A row stays once written: a version someone may have
-- accepted remains acceptable even if the site is down later or the file drops it.
CREATE TABLE legal_versions (
    doc           TEXT NOT NULL CHECK (doc IN ('terms', 'privacy')),
    version       TEXT NOT NULL CHECK (version ~ '^[0-9]{8}$'),
    first_seen_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (doc, version)
);
