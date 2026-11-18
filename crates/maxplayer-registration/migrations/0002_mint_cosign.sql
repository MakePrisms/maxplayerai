-- Cosigned mint links (`maxplayer account link --mint`, README "Cosigned mint link").
--
-- A mint operator links the mint server's key to the account they already have by having BOTH
-- keys sign: the already email-linked key (the "account" key) and the mint key. No second email
-- code. The mint's row copies the account's email and accepted terms/privacy versions, records
-- `code_entered_by = 'cosigned'` and names the account key in `linked_by`.

ALTER TABLE registrations DROP CONSTRAINT registrations_code_entered_by_check;
ALTER TABLE registrations ADD CONSTRAINT registrations_code_entered_by_check
    CHECK (code_entered_by IN ('human', 'agent', 'cosigned'));
ALTER TABLE registrations ADD COLUMN linked_by TEXT CHECK (linked_by ~ '^[0-9a-f]{64}$');
-- Exactly the cosigned rows name the key that vouched for them.
ALTER TABLE registrations ADD CONSTRAINT registrations_cosigned_names_linked_by
    CHECK ((code_entered_by = 'cosigned') = (linked_by IS NOT NULL));

-- One random challenge per (account key, mint key) attempt. Single use (`used_at`), short-lived
-- (`limits.mint_challenge_ttl_secs`, default 5 minutes), and bound to both keys: the confirm must
-- be signed by exactly these two keys. Not secret on its own (both signatures are needed).
CREATE TABLE mint_link_challenges (
    challenge      TEXT PRIMARY KEY CHECK (challenge ~ '^[0-9a-f]{64}$'),
    account_pubkey TEXT NOT NULL CHECK (account_pubkey ~ '^[0-9a-f]{64}$'),
    mint_pubkey    TEXT NOT NULL CHECK (mint_pubkey ~ '^[0-9a-f]{64}$'),
    expires_at     TIMESTAMPTZ NOT NULL,
    used_at        TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (account_pubkey <> mint_pubkey)
);
CREATE INDEX mint_link_challenges_pair_idx ON mint_link_challenges (account_pubkey, mint_pubkey);
CREATE INDEX mint_link_challenges_created_idx ON mint_link_challenges (created_at);
