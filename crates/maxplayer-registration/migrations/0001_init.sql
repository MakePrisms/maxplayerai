-- Registration database (docs: crates/maxplayer-registration/README.md, spec §5).
-- Runs as the service's own non-superuser role, which owns its own database.
-- Emails are stored trimmed + lowercased. No table here is ever copied into relay events.

-- One row per registered key. One email with five agents is five rows.
CREATE TABLE registrations (
    pubkey          TEXT PRIMARY KEY CHECK (pubkey ~ '^[0-9a-f]{64}$'),
    email           TEXT NOT NULL,
    label           TEXT NOT NULL,
    terms_version   TEXT NOT NULL,
    privacy_version TEXT NOT NULL,
    accepted_at     TIMESTAMPTZ NOT NULL,
    code_entered_by TEXT NOT NULL CHECK (code_entered_by IN ('human', 'agent')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX registrations_email_idx ON registrations (email);

-- Codes are stored only as a keyed hash (HMAC-SHA256 with a server-side pepper),
-- bound to the key they were issued for. The versions shown in the code email are
-- kept with the code so the confirm step records exactly what was accepted.
CREATE TABLE email_codes (
    id              BIGSERIAL PRIMARY KEY,
    email           TEXT NOT NULL,
    code_hash       BYTEA NOT NULL,
    purpose         TEXT NOT NULL DEFAULT 'link' CHECK (purpose IN ('link')),
    pubkey          TEXT NOT NULL CHECK (pubkey ~ '^[0-9a-f]{64}$'),
    label           TEXT NOT NULL,
    terms_version   TEXT NOT NULL,
    privacy_version TEXT NOT NULL,
    notice_version  TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    expires_at      TIMESTAMPTZ NOT NULL,
    used_at         TIMESTAMPTZ,
    -- Set when a newer code for the same key replaces this one.
    revoked_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX email_codes_pubkey_idx ON email_codes (pubkey, created_at DESC);
CREATE INDEX email_codes_created_idx ON email_codes (created_at);

-- NIP-43 commands still to send. Retried until the relay confirms them.
CREATE TABLE member_sync_outbox (
    id              BIGSERIAL PRIMARY KEY,
    pubkey          TEXT NOT NULL CHECK (pubkey ~ '^[0-9a-f]{64}$'),
    action          TEXT NOT NULL CHECK (action IN ('add', 'remove')),
    status          TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'done', 'superseded')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    lease_until     TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    done_at         TIMESTAMPTZ
);
CREATE UNIQUE INDEX member_sync_outbox_one_pending
    ON member_sync_outbox (pubkey, action) WHERE status = 'pending';
CREATE INDEX member_sync_outbox_due_idx
    ON member_sync_outbox (next_attempt_at) WHERE status = 'pending';

-- Keys this service has added to the relay member list. The reconcile removes a key
-- that is here but no longer in `registrations` (operator removal = delete the row),
-- and never touches members it did not add unless `sync.reconcile_removals` is on.
CREATE TABLE managed_members (
    pubkey         TEXT PRIMARY KEY CHECK (pubkey ~ '^[0-9a-f]{64}$'),
    first_added_at TIMESTAMPTZ NOT NULL
);

-- From Resend webhooks, so we know who never got a notice.
CREATE TABLE email_bounces (
    id          BIGSERIAL PRIMARY KEY,
    email       TEXT NOT NULL,
    bounced_at  TIMESTAMPTZ NOT NULL,
    event_type  TEXT NOT NULL,
    webhook_id  TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (webhook_id, email)
);
CREATE INDEX email_bounces_email_idx ON email_bounces (email);

-- One row per email, created at signup (automatic opt-in, spec §8.1).
CREATE TABLE marketing_consents (
    email              TEXT PRIMARY KEY,
    opted_in_at        TIMESTAMPTZ NOT NULL,
    method             TEXT NOT NULL CHECK (method IN ('signup_default', 'cli')),
    notice_version     TEXT NOT NULL,
    unsubscribed_at    TIMESTAMPTZ,
    unsubscribe_method TEXT CHECK (unsubscribe_method IN ('cli', 'link', 'reply', 'complaint')),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Short-lived rate-limit counters. Buckets are "ip:<addr>", "pk:<hex>" or
-- "email:<sha256 of the email>", never a raw address.
CREATE TABLE rate_events (
    bucket     TEXT NOT NULL,
    action     TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX rate_events_lookup_idx ON rate_events (bucket, action, created_at);
CREATE INDEX rate_events_created_idx ON rate_events (created_at);

-- NIP-98 event ids already used, so a captured Authorization header cannot be replayed.
CREATE TABLE nip98_seen (
    event_id TEXT PRIMARY KEY,
    seen_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Resend (Svix) webhook message ids already handled.
CREATE TABLE webhook_events (
    webhook_id  TEXT PRIMARY KEY,
    event_type  TEXT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
