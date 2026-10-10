-- SuperFrontierLiquid application schema, version 1.
--
-- Conventions, applied uniformly:
--
--   * Money is TEXT holding a normalised `rust_decimal` string. See the
--     `codec` module for why, and for why no query in this schema may
--     aggregate or order by one of these columns in SQL.
--   * Instants are INTEGER microseconds since the Unix epoch. RFC3339 text
--     does not compare correctly under `<` when fractional-second precision
--     varies ('Z' sorts above '.'), and every range query here depends on
--     `<` over time.
--   * Booleans are INTEGER 0/1.
--   * Every index carries a comment naming the repository method it exists
--     for. An index without a caller is a write cost with no reader.

-- ---------------------------------------------------------------- users ----

CREATE TABLE users (
    user_id       TEXT    NOT NULL PRIMARY KEY,
    -- The handle as the player typed it; shown back to them verbatim.
    handle        TEXT    NOT NULL,
    -- The same handle lowercased, written in the same INSERT. This column,
    -- not application code, is what makes handle uniqueness case-insensitive.
    handle_lower  TEXT    NOT NULL,
    address       TEXT    NOT NULL,
    recovery_hash TEXT    NOT NULL,
    is_bot        INTEGER NOT NULL CHECK (is_bot IN (0, 1)),
    created_at    INTEGER NOT NULL,
    last_seen_at  INTEGER NOT NULL
) STRICT;

-- UserRepository::create (the uniqueness guarantee itself) and
-- UserRepository::by_handle. A check-then-insert in the gateway would race
-- between two joins of "Alice" and "alice"; this index cannot.
CREATE UNIQUE INDEX ux_users_handle_lower ON users (handle_lower);

-- UserRepository::by_address. Unique because the gateway mints one custodial
-- address per account, and two users sharing one would mean crossed funds.
CREATE UNIQUE INDEX ux_users_address ON users (address);

-- UserRepository::list / count with include_bots = true (oldest first,
-- user_id as the tie-break so paging is stable when two rows share a
-- microsecond).
CREATE INDEX ix_users_created_at ON users (created_at, user_id);

-- UserRepository::list / count with include_bots = false. Partial, because
-- the bot population is expected to dwarf the human one and a full index
-- would make the humans-only scan read mostly bots.
CREATE INDEX ix_users_human_created_at
    ON users (created_at, user_id) WHERE is_bot = 0;

-- ------------------------------------------------------------- sessions ----

CREATE TABLE sessions (
    token_hash TEXT    NOT NULL PRIMARY KEY,
    -- ON DELETE CASCADE: a session is worthless without its user, and
    -- leaving orphans would let a deleted account keep authenticating.
    user_id    TEXT    NOT NULL REFERENCES users (user_id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked    INTEGER NOT NULL CHECK (revoked IN (0, 1))
) STRICT;

-- SessionRepository::revoke_all_for_user.
CREATE INDEX ix_sessions_user_id ON sessions (user_id);

-- SessionRepository::delete_expired.
CREATE INDEX ix_sessions_expires_at ON sessions (expires_at);

-- ------------------------------------------------------- faucet claims ----

-- One claim can grant several assets, and each becomes a row here. The natural
-- key is therefore (claim_id, asset), not claim_id alone: a single-column key
-- makes the second asset of a multi-asset grant collide with the first, which
-- fails the claim *after* the deposits have already been minted on chain. The
-- caller then sees an error and has the money anyway.
CREATE TABLE faucet_claims (
    claim_id   TEXT    NOT NULL,
    -- ON DELETE RESTRICT: the faucet is the only mint in the system, so
    -- these rows are what conservation checks reconcile against. Deleting a
    -- user must not silently destroy evidence that units were created.
    user_id    TEXT    NOT NULL REFERENCES users (user_id) ON DELETE RESTRICT,
    asset      TEXT    NOT NULL,
    amount     TEXT    NOT NULL,
    tx_hash    TEXT    NOT NULL,
    claimed_at INTEGER NOT NULL,
    PRIMARY KEY (claim_id, asset)
) STRICT;

-- FaucetRepository::last_claim_at, ::claim_count and ::history. The DESC on
-- claimed_at matches "newest first" so history needs no sort step.
CREATE INDEX ix_faucet_user_claimed
    ON faucet_claims (user_id, claimed_at DESC);

-- FaucetRepository::total_granted, which groups by asset. Covering: the scan
-- reads asset and amount from the index without touching the table.
CREATE INDEX ix_faucet_asset_amount ON faucet_claims (asset, amount);

-- -------------------------------------------------------------- candles ----

CREATE TABLE candles (
    symbol      TEXT    NOT NULL,
    interval    TEXT    NOT NULL,
    open_time   INTEGER NOT NULL,
    open        TEXT    NOT NULL,
    high        TEXT    NOT NULL,
    low         TEXT    NOT NULL,
    close       TEXT    NOT NULL,
    volume      TEXT    NOT NULL,
    trade_count INTEGER NOT NULL,
    -- The natural key *is* the identity of a bar. Making it the primary key
    -- is what lets CandleRepository::upsert be a single ON CONFLICT statement
    -- rather than a read-modify-write, and it is what makes re-aggregating a
    -- bucket idempotent.
    PRIMARY KEY (symbol, interval, open_time)
) STRICT;

-- CandleRepository::prune, which filters on interval alone across all
-- symbols. The primary key leads with symbol, so it cannot serve this.
CREATE INDEX ix_candles_interval_open_time ON candles (interval, open_time);

-- -------------------------------------------------------- ledger entries ---

CREATE TABLE ledger_entries (
    entry_id      TEXT    NOT NULL PRIMARY KEY,
    -- ON DELETE RESTRICT, for the same reason as faucet_claims: this is the
    -- audit trail of value movement and it must outlive an account deletion.
    user_id       TEXT    NOT NULL REFERENCES users (user_id) ON DELETE RESTRICT,
    kind          TEXT    NOT NULL,
    asset         TEXT    NOT NULL,
    amount        TEXT    NOT NULL,
    balance_after TEXT    NOT NULL,
    memo          TEXT,
    tx_hash       TEXT,
    height        INTEGER,
    at            INTEGER NOT NULL
) STRICT;

-- LedgerRepository::for_user with no kind filter.
CREATE INDEX ix_ledger_user_at ON ledger_entries (user_id, at DESC);

-- LedgerRepository::for_user with a kind filter.
CREATE INDEX ix_ledger_user_kind_at ON ledger_entries (user_id, kind, at DESC);

-- LedgerRepository::net_by_asset: a half-open window on `at`, grouped by
-- asset. Ascending on `at` because this one is a range scan, not a
-- newest-first read.
CREATE INDEX ix_ledger_user_at_asset
    ON ledger_entries (user_id, at, asset, amount);

-- ---------------------------------------------------------- leaderboard ----

-- No foreign key to users. A leaderboard row is a snapshot of a ranking as
-- it stood at `computed_at`; it stays meaningful even if the account behind
-- it later disappears, and a cascade would silently rewrite history.
--
-- `window` is a SQLite keyword (window functions), hence `window_key`.
CREATE TABLE leaderboard (
    window_key TEXT    NOT NULL,
    rank       INTEGER NOT NULL,
    handle     TEXT    NOT NULL,
    address    TEXT    NOT NULL,
    equity     TEXT    NOT NULL,
    pnl        TEXT    NOT NULL,
    pnl_pct    TEXT    NOT NULL,
    volume     TEXT    NOT NULL,
    trades     INTEGER NOT NULL,
    is_bot     INTEGER NOT NULL CHECK (is_bot IN (0, 1)),
    -- LeaderboardRepository::top reads this in primary-key order, so the top
    -- N is a prefix scan with no sort.
    PRIMARY KEY (window_key, rank)
) STRICT;

-- LeaderboardRepository::for_handle.
CREATE INDEX ix_leaderboard_window_handle ON leaderboard (window_key, handle);

-- Kept apart from `leaderboard` so that LeaderboardRepository::computed_at
-- stays answerable for a window whose ranking is legitimately empty.
CREATE TABLE leaderboard_meta (
    window_key  TEXT    NOT NULL PRIMARY KEY,
    computed_at INTEGER NOT NULL
) STRICT;

-- ---------------------------------------------------------- market meta ----

-- No foreign key from candles to this table: candle aggregation can begin
-- before the gateway has cached the market list, and a missing cache row is
-- not a reason to drop market data.
CREATE TABLE market_meta (
    symbol                   TEXT    NOT NULL PRIMARY KEY,
    kind                     TEXT    NOT NULL,
    asset_class              TEXT    NOT NULL,
    base                     TEXT    NOT NULL,
    quote                    TEXT    NOT NULL,
    display_name             TEXT    NOT NULL,
    status                   TEXT    NOT NULL,
    tick_size                TEXT    NOT NULL,
    lot_size                 TEXT    NOT NULL,
    min_notional             TEXT    NOT NULL,
    price_decimals           INTEGER NOT NULL,
    size_decimals            INTEGER NOT NULL,
    max_leverage             TEXT,
    initial_margin_bps       INTEGER,
    maintenance_margin_bps   INTEGER,
    funding_interval_seconds INTEGER,
    expiry                   INTEGER,
    maker_fee_bps            INTEGER NOT NULL,
    taker_fee_bps            INTEGER NOT NULL
) STRICT;

-- MarketMetaRepository::all, ::by_symbol and ::set_status are all served by
-- the primary key; the universe is a few dozen rows and needs no other index.

-- ------------------------------------------------------------ audit log ----

-- No foreign key on `actor`. Some actions have a system actor with no user
-- row, and the audit trail must survive the deletion of whoever it indicts.
CREATE TABLE audit_log (
    record_id TEXT    NOT NULL PRIMARY KEY,
    actor     TEXT,
    "action"  TEXT    NOT NULL,
    subject   TEXT    NOT NULL,
    detail    TEXT    NOT NULL,
    source_ip TEXT,
    at        INTEGER NOT NULL
) STRICT;

-- AuditRepository::for_subject.
CREATE INDEX ix_audit_subject_at ON audit_log (subject, at DESC);

-- AuditRepository::for_actor. Partial: rows with a NULL actor are system
-- actions and can never match a for_actor lookup.
CREATE INDEX ix_audit_actor_at ON audit_log (actor, at DESC) WHERE actor IS NOT NULL;
