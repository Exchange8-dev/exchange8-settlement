// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:Settlement
// SPDX-License-Identifier: Apache-2.0

//! Exchange8 SQL integration tests.
//!
//! Runs against the live platform PostgreSQL (18) using the
//! `exchange8_probe` non-bypass role — the RLS matrix:
//!   1. no tenant context → 0 rows (silent enforcement)
//!   2. own env context → own rows only
//!   3. cross-env → 0 rows
//! Plus the schema-level uniqueness backstop and the service-role DCR
//! check.
//!
//! Skips (passes silently) when the DB is unreachable so `cargo test`
//! stays hermetic offline.
//!
//! Design: `services/settlement/DESIGN.md` §9 (testing strategy).

use sqlx::{Connection, PgPool};
use uuid::Uuid;

const PROBE_URL: &str = "postgresql://exchange8_probe@localhost:5432/exchange8";

fn env_a() -> Uuid {
    Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()
}

fn env_b() -> Uuid {
    Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap()
}

async fn probe_conn() -> Option<sqlx::pool::PoolConnection<sqlx::Postgres>> {
    // NOTE (architecture-principles.md §4.3d): tenant context is
    // connection-local — every test acquires ONE connection and does all
    // statements on it (never pool connection A for context, B for data).
    let pool = PgPool::connect(PROBE_URL).await.ok()?;
    pool.acquire().await.ok()
}

/// Set tenant context inside the test's transaction.
async fn set_ctx(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, env: Uuid) {
    sqlx::query("SELECT sentin8_set_tenant_context($1)")
        .bind(env)
        .execute(&mut **tx)
        .await
        .unwrap();
}

/// RLS matrix: context-not-set → 0 rows.
#[tokio::test]
async fn rls_matrix_no_context_returns_zero() {
    let Some(mut conn) = probe_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM clearing.rls_probe")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(n, 0, "no tenant context must see zero rows");
}

/// RLS matrix: own-env context → own row.
#[tokio::test]
async fn rls_matrix_own_env_sees_own_row() {
    let Some(mut conn) = probe_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    let mut tx = conn.begin().await.unwrap();
    set_ctx(&mut tx, env_a()).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM clearing.rls_probe")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 1, "env-a must see exactly its own row");
    tx.rollback().await.unwrap();
}

/// RLS matrix: cross-env → 0 rows.
#[tokio::test]
async fn rls_matrix_cross_env_returns_zero() {
    let Some(mut conn) = probe_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    let mut tx = conn.begin().await.unwrap();
    set_ctx(&mut tx, env_b()).await;
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM clearing.rls_probe WHERE label = 'env-a'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(n, 0, "env-b must not see env-a's rows");
    // And it sees its own.
    let own: i64 = sqlx::query_scalar("SELECT count(*) FROM clearing.rls_probe")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(own, 1);
    tx.rollback().await.unwrap();
}

/// Settlement-owned idempotency substrate: novation is unique per
/// (env, trade) — a re-novation of the same trade never produces a second
/// row (the engine dedupes in-memory; the schema is the backstop).
#[tokio::test]
async fn novation_unique_per_env_trade() {
    let Some(mut conn) = probe_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    let mut tx = conn.begin().await.unwrap();
    set_ctx(&mut tx, env_a()).await;
    sqlx::query(
        "INSERT INTO settlement.novation (env_id, trade_id, novation_seq) \
         VALUES ($1, 9100000001, 1)",
    )
    .bind(env_a())
    .execute(&mut *tx)
    .await
    .unwrap();
    let res = sqlx::query(
        "INSERT INTO settlement.novation (env_id, trade_id, novation_seq) \
         VALUES ($1, 9100000001, 2)",
    )
    .bind(env_a())
    .execute(&mut *tx)
    .await;
    assert!(
        res.is_err(),
        "second novation of one trade must violate the uniqueness backstop"
    );
    // Roll back: the matrix must not leave probe rows on the live schema.
    tx.rollback().await.unwrap();
}

/// Every exchange8 service role exists, non-super, non-bypass — read
/// directly from pg_roles (no schema-privilege indirection).
#[tokio::test]
async fn service_roles_exist_non_bypass() {
    let Some(mut conn) = probe_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    let expected = [
        "exchange8_refdata_service",
        "exchange8_gateway_service",
        "exchange8_me_service",
        "exchange8_oms_service",
        "exchange8_risk_service",
        "exchange8_md_service",
        "exchange8_vfeeds_service",
        "exchange8_sor_service",
        "exchange8_rfq_service",
        "exchange8_clearing_service",
        "exchange8_margin_service",
        "exchange8_settlement_service",
        "exchange8_regulatory_service",
        "exchange8_surveillance_service",
    ];
    for role in expected {
        let row: Option<(bool, bool)> = sqlx::query_as(
            "SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = $1",
        )
        .bind(role)
        .fetch_optional(&mut *conn)
        .await
        .unwrap();
        let (superuser, bypass) =
            row.unwrap_or_else(|| panic!("service role {role} does not exist (DCR)"));
        assert!(!superuser, "service role {role} must not be superuser");
        assert!(!bypass, "service role {role} must not bypass RLS");
    }
}
