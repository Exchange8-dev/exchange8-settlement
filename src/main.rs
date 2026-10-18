// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:Settlement
// SPDX-License-Identifier: Apache-2.0

//! Exchange8:SETTLEMENT service binary — novation, custody, collateral, give-up, block trades, EOD files.
//!
//! Exchange8:SETTLEMENT service binary — novation, custody, collateral, give-up, block trades, EOD files.
//!
//! Design: `services/settlement/DESIGN.md`. Core logic in the sibling modules;
//! this shell wires the tokio HTTP/TLS surface via the platform
//! `sentin8-shell` crate (registration, DB pool, health).

#![allow(dead_code)]

// (master design §11.1). Workers (bus consumers, session
// supervisors, purge jobs) are in-process tokio tasks, so the
mod ccp_files;
mod crypto_ext;
mod fx_core;
mod options;
mod openapi;
mod registration;
mod settlement;

fn extend_router(
    app: axum::Router<std::sync::Arc<sentin8::shell::AppState>>,
    _state: std::sync::Arc<sentin8::shell::AppState>,
) -> axum::Router<std::sync::Arc<sentin8::shell::AppState>> {
    // ── Shield policy-admin API (services/settlement/DESIGN.md §4 —
    // shared implementation; the shell AppState implements HasPolicyAdmin).
    sentin8::shell::mount_policy_admin(
        // EOD file jobs run on Sentin8:Cron; custody adapters are trait impls
        // (services/settlement/DESIGN.md §2, §10); the HTTP router carries
        // health + admin endpoints only.
        app,
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    sentin8::shell::serve(
        sentin8::shell::Sentin8Shell::new("exchange8:settlement", "EXCHANGE8_SETTLEMENT")
            .with_registration(registration::print)
            .with_simple_state()
            .with_router(extend_router),
    )
    .await
}
