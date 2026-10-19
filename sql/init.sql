-- SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
-- SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
-- SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:Settlement
-- SPDX-License-Identifier: Apache-2.0

-- Ex8:Settlement - clearing assignments, CCP, custody, collateral, give-ups,
-- block trades, crypto assets.
--
-- Design: design/sentin8-exchange8-data-model-technical-design.md §3.7 (ST).
-- No migration code (new platform): `sentin8-cli vertical init` applies this.

CREATE SCHEMA IF NOT EXISTS settlement;

-- Participant clearing assignment (legacy CLEARING).
CREATE TABLE IF NOT EXISTS settlement.clearing (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    market_code     TEXT NOT NULL,
    connection_id   TEXT NOT NULL,
    clearing_id     TEXT NOT NULL,
    ccp_facility_id  UUID,
    status          TEXT NOT NULL DEFAULT 'E' CHECK (status IN ('E','D')),
    UNIQUE (env_id, market_code, connection_id, clearing_id)
);

-- CCP endpoints + export credentials (secret refs only).
CREATE TABLE IF NOT EXISTS settlement.ccp_facility (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    name        TEXT NOT NULL,
    api_url     TEXT,
    UNIQUE (env_id, name)
);
CREATE TABLE IF NOT EXISTS settlement.ccp_facility_file (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    ccp_facility_id UUID NOT NULL REFERENCES settlement.ccp_facility(id),
    file_type   TEXT NOT NULL,           -- CDS/SIS/Fortis file family
    creds_secret_ref TEXT,
    schedule    TEXT
);

-- Securities cleared where.
CREATE TABLE IF NOT EXISTS settlement.security_to_ccp (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    security_id UUID NOT NULL,
    ccp_facility_id UUID NOT NULL REFERENCES settlement.ccp_facility(id)
);

-- Per-trade CCP novation record (idempotent per trade).
CREATE TABLE IF NOT EXISTS settlement.novation (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    trade_id    BIGINT NOT NULL,
    novation_seq BIGINT NOT NULL,
    novated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (env_id, trade_id)
);

-- Crypto asset master (chain, contract address, decimals).
CREATE TABLE IF NOT EXISTS settlement.asset (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    symbol          TEXT NOT NULL,
    chain           TEXT NOT NULL,        -- bitcoin/ethereum/solana/...
    contract_address TEXT,                -- NULL for native chains
    decimals        INTEGER NOT NULL,
    UNIQUE (env_id, symbol, chain)
);

-- Per-env custody account (address + creds secret ref).
CREATE TABLE IF NOT EXISTS settlement.custody_account (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    asset_id    UUID NOT NULL REFERENCES settlement.asset(id),
    address     TEXT NOT NULL,
    balance     NUMERIC(24,8) NOT NULL DEFAULT 0,
    creds_secret_ref TEXT,               -- private keys NEVER in-process
    UNIQUE (env_id, asset_id, address)
);

-- On-chain transaction ledger.
CREATE TABLE IF NOT EXISTS settlement.custody_transaction (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    custody_account_id UUID NOT NULL REFERENCES settlement.custody_account(id),
    txid        TEXT,
    direction   TEXT NOT NULL CHECK (direction IN ('deposit','withdraw')),
    amount      NUMERIC(24,8) NOT NULL,
    state       TEXT NOT NULL DEFAULT 'pending'
                CHECK (state IN ('pending','signed','broadcast','confirmed','failed')),
    ts          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_exchange8_custodytx_env_state ON settlement.custody_transaction (env_id, state);

-- Collateral balances + haircuts.
CREATE TABLE IF NOT EXISTS settlement.collateral (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    account_id  TEXT NOT NULL,
    asset_kind  TEXT NOT NULL CHECK (asset_kind IN ('cash','security','crypto')),
    asset_ref   TEXT NOT NULL,            -- currency code / security id / asset id
    market_value NUMERIC(20,8) NOT NULL DEFAULT 0,
    haircut_bps INTEGER NOT NULL DEFAULT 0,
    bucket      TEXT NOT NULL DEFAULT 'cash',
    concentration_bps INTEGER NOT NULL DEFAULT 10000,
    rehypothecatable BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE INDEX IF NOT EXISTS idx_exchange8_collateral_env ON settlement.collateral (env_id, account_id);

-- Give-up/give-in allocations.
CREATE TABLE IF NOT EXISTS settlement.giveup (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    trade_id        BIGINT NOT NULL,
    executing_broker TEXT NOT NULL,
    clearing_broker TEXT NOT NULL,
    instrument_id   UUID,
    qty             NUMERIC(18,8) NOT NULL,
    price           NUMERIC(18,8) NOT NULL,
    accepted        BOOLEAN NOT NULL DEFAULT FALSE,
    ts              TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Block / EFP / EFR trades (reported, not CLOB-matched).
CREATE TABLE IF NOT EXISTS settlement.block_trade (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    block_type  TEXT NOT NULL CHECK (block_type IN ('block','efp','efr')),
    instrument_id UUID NOT NULL,
    qty         NUMERIC(18,8) NOT NULL,
    price       NUMERIC(18,8) NOT NULL,
    buyer       TEXT NOT NULL,
    seller      TEXT NOT NULL,
    reported_by TEXT NOT NULL,
    report_delay_s INTEGER,
    state       TEXT NOT NULL DEFAULT 'accepted' CHECK (state IN
                ('accepted','rejected','too_small','off_market','late')),
    ts          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- RLS (architecture-principles.md §4.3): enable + force on all tenant tables.
-- Policies use the platform helpers sentin8_context_initialized() and
-- sentin8_current_env_id(); services call set_tenant_context on the same
-- connection as the data query.
-- ---------------------------------------------------------------------------
DO $$
DECLARE tbl TEXT;
BEGIN
    FOREACH tbl IN ARRAY ARRAY['settlement.clearing', 'settlement.ccp_facility', 'settlement.ccp_facility_file', 'settlement.security_to_ccp', 'settlement.novation', 'settlement.asset', 'settlement.custody_account', 'settlement.custody_transaction', 'settlement.collateral', 'settlement.giveup', 'settlement.block_trade'] LOOP
        BEGIN
            EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', tbl);
            EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', tbl);
            EXECUTE format('DROP POLICY IF EXISTS %1$I_select ON %2$s;
CREATE POLICY %1$I_select ON %2$s FOR SELECT USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', split_part(tbl, '.', 2), tbl);
            EXECUTE format('DROP POLICY IF EXISTS %1$I_insert ON %2$s;
CREATE POLICY %1$I_insert ON %2$s FOR INSERT WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', split_part(tbl, '.', 2), tbl);
            EXECUTE format('DROP POLICY IF EXISTS %1$I_update ON %2$s;
CREATE POLICY %1$I_update ON %2$s FOR UPDATE USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', split_part(tbl, '.', 2), tbl);
            EXECUTE format('DROP POLICY IF EXISTS %1$I_delete ON %2$s;
CREATE POLICY %1$I_delete ON %2$s FOR DELETE USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', split_part(tbl, '.', 2), tbl);
        EXCEPTION WHEN OTHERS THEN
            RAISE NOTICE 'RLS setup failed for %: %', tbl, SQLERRM;
        END;
    END LOOP;
END $$;

-- ---------------------------------------------------------------------------
-- Per-service role grants (schema-per-service: this service owns schema
-- `settlement`; grants live in db `exchange8` only - design doc §1).
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'exchange8_settlement_service') THEN
        ALTER ROLE exchange8_settlement_service SET search_path = settlement, public;
        GRANT USAGE ON SCHEMA settlement TO exchange8_settlement_service;
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA settlement TO exchange8_settlement_service;
        GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA settlement TO exchange8_settlement_service;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- Local outbox (DBX §6.7 - generated from `sentin8_outbox::outbox_table_sql(
-- "settlement")`): rows written in the same DB transaction as the domain change;
-- the shell's relay (boot step §8b) pushes them to Bus `POST /v1/produce`.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS settlement.outbox (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    event_type      TEXT NOT NULL,
    stream_family   TEXT NOT NULL,
    payload         JSONB NOT NULL,
    publisher_id_token  TEXT,
    publisher_subject   TEXT,
    relayed         BOOLEAN NOT NULL DEFAULT false,
    relayed_at      TIMESTAMPTZ,
    relay_attempts  INT NOT NULL DEFAULT 0,
    next_retry_at   TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_settlement_outbox_unrelayed
    ON settlement.outbox (created_at) WHERE relayed = false;
DO $$ BEGIN
    EXECUTE 'ALTER TABLE settlement.outbox ENABLE ROW LEVEL SECURITY';
    EXECUTE 'ALTER TABLE settlement.outbox FORCE ROW LEVEL SECURITY';
    EXECUTE 'DROP POLICY IF EXISTS outbox_select ON settlement.outbox';
    EXECUTE 'CREATE POLICY outbox_select ON settlement.outbox FOR SELECT
        USING (sentin8_context_initialized()
               AND env_id = sentin8_current_env_id())';
    EXECUTE 'DROP POLICY IF EXISTS outbox_insert ON settlement.outbox';
    EXECUTE 'CREATE POLICY outbox_insert ON settlement.outbox FOR INSERT
        WITH CHECK (sentin8_context_initialized()
               AND env_id = sentin8_current_env_id())';
    EXECUTE 'DROP POLICY IF EXISTS outbox_update ON settlement.outbox';
    EXECUTE 'CREATE POLICY outbox_update ON settlement.outbox FOR UPDATE
        USING (sentin8_context_initialized()
               AND env_id = sentin8_current_env_id())';
EXCEPTION WHEN OTHERS THEN
    RAISE NOTICE 'RLS setup failed for settlement.outbox: %', SQLERRM;
END $$;

CREATE OR REPLACE FUNCTION settlement.settlement_notify_outbox() RETURNS TRIGGER
LANGUAGE plpgsql AS $fn$
BEGIN
    PERFORM pg_notify('settlement_outbox_changed', '');
    RETURN NEW;
END;
$fn$;
DROP TRIGGER IF EXISTS outbox_notify_trigger ON settlement.outbox;
CREATE TRIGGER outbox_notify_trigger
    AFTER INSERT ON settlement.outbox
    FOR EACH ROW
    EXECUTE FUNCTION settlement.settlement_notify_outbox();

-- ---------------------------------------------------------------------------
-- Local audit journal (DBX §6.8 - generated from
-- `sentin8_audit::audit_journal_sql("settlement")`): rows written in the same
-- transaction as the domain change; the shell's pusher (boot step §8c) pushes
-- them to Sys `POST /v1/audit` (`sentin8:sys:audit:write`). A Sys outage
-- never drops audit.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS settlement.audit_journal (
    id              UUID PRIMARY KEY,
    env_id          UUID NOT NULL,
    action          TEXT NOT NULL,
    target_resource_type TEXT,
    target_resource_id    TEXT,
    actor_user_id   TEXT,
    actor_subject_type TEXT,
    severity        TEXT NOT NULL DEFAULT 'info',
    correlation_id  TEXT,
    before_state    JSONB,
    after_state     JSONB,
    details         JSONB,
    pushed          BOOLEAN NOT NULL DEFAULT false,
    pushed_at       TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_settlement_audit_journal_unpushed
    ON settlement.audit_journal (created_at) WHERE pushed = false;
DO $$ BEGIN
    EXECUTE 'ALTER TABLE settlement.audit_journal ENABLE ROW LEVEL SECURITY';
    EXECUTE 'ALTER TABLE settlement.audit_journal FORCE ROW LEVEL SECURITY';
    EXECUTE 'DROP POLICY IF EXISTS audit_journal_dml ON settlement.audit_journal';
    EXECUTE 'CREATE POLICY audit_journal_dml ON settlement.audit_journal
        USING (sentin8_context_initialized()
               AND env_id = sentin8_current_env_id())
        WITH CHECK (sentin8_context_initialized()
               AND env_id = sentin8_current_env_id())';
EXCEPTION WHEN OTHERS THEN
    RAISE NOTICE 'RLS setup failed for settlement.audit_journal: %', SQLERRM;
END $$;

-- Outbox + journal role grants (same pattern as the table grants above).
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'exchange8_settlement_service') THEN
        GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA settlement TO exchange8_settlement_service;
    END IF;
END $$;
