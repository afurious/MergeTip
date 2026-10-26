//! Receiver payout methods and the fiat receive form (task 5.3).

mod common;

use std::str::FromStr;

use axum::http::StatusCode;
use common::{fake_app, fake_fiat, fiat_app, send, session_for};
use mergetip_store::{tips, users};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn fiat_endpoints_are_off_until_enabled(pool: PgPool) -> sqlx::Result<()> {
    let session = session_for(&pool, 11, "reviewer").await;
    // No provider configured.
    let app = fake_app(pool.clone());
    let (status, body) = send(&app, "GET", "/v1/payouts/countries", Some(&session), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "fiat_disabled");
    // Provider configured, runtime flag off.
    let app = fiat_app(pool.clone(), fake_fiat()).await;
    mergetip_store::feature_flags::set(&pool, "fiat_payouts", false)
        .await
        .unwrap();
    let (status, _) = send(&app, "GET", "/v1/payouts/countries", Some(&session), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_receive_form_comes_from_the_providers_requirements(pool: PgPool) -> sqlx::Result<()> {
    let app = fiat_app(pool.clone(), fake_fiat()).await;
    let session = session_for(&pool, 11, "reviewer").await;

    let (status, body) = send(&app, "GET", "/v1/payouts/countries", Some(&session), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["code"], "NG");
    assert_eq!(
        body[1]["corridors"][0]["destination_types"][0],
        "mobile_money"
    );

    let (status, body) = send(
        &app,
        "GET",
        "/v1/payouts/countries/ng",
        Some(&session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let form = &body["forms"][0];
    assert_eq!(form["destination_type"], "bank");
    assert_eq!(form["fields"][0]["key"], "account_number");
    assert_eq!(form["fields"][0]["min_length"], 10);
    assert_eq!(form["banks"][0], json!({ "code": "058", "name": "GTBank" }));

    let (status, body) = send(
        &app,
        "GET",
        "/v1/payouts/countries/KE",
        Some(&session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "fiat_unsupported");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_verified_account_is_saved_encrypted_and_shown_masked(pool: PgPool) -> sqlx::Result<()> {
    let app = fiat_app(pool.clone(), fake_fiat()).await;
    let session = session_for(&pool, 11, "reviewer").await;

    let (status, body) = send(
        &app,
        "POST",
        "/v1/me/payout-methods",
        Some(&session),
        Some(json!({
            "country": "NG", "destination_type": "bank", "is_default": true,
            "fields": { "account_number": "0123456789", "bank_code": "058" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["account"], "****6789");
    assert_eq!(body["currency"], "NGN");
    assert_eq!(body["is_default"], true);
    let text = body.to_string();
    assert!(
        !text.contains("0123456789") && !text.contains("Ada"),
        "{text}"
    );

    let stored = sqlx::query_scalar!("SELECT beneficiary_ciphertext FROM payout_methods")
        .fetch_one(&pool)
        .await?
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&stored).contains("0123456789"),
        "only ciphertext at rest"
    );

    let (status, list) = send(&app, "GET", "/v1/me/payout-methods", Some(&session), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(!list.to_string().contains("0123456789"));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn unverifiable_or_unsupported_accounts_are_refused(pool: PgPool) -> sqlx::Result<()> {
    let app = fiat_app(pool.clone(), fake_fiat()).await;
    let session = session_for(&pool, 11, "reviewer").await;
    let add = |body| {
        let app = app.clone();
        let session = session.clone();
        async move {
            send(
                &app,
                "POST",
                "/v1/me/payout-methods",
                Some(&session),
                Some(body),
            )
            .await
        }
    };

    let (status, body) = add(json!({
        "country": "NG", "destination_type": "bank",
        "fields": { "account_number": "9999999999", "bank_code": "058" }
    }))
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "account_lookup_failed");
    let (status, body) = add(json!({
        "country": "NG", "destination_type": "mobile_money",
        "fields": { "account_number": "0123456789", "network": "MTN" }
    }))
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "fiat_unsupported");
    let (status, body) = add(json!({
        "country": "NG", "destination_type": "bank", "fields": {}
    }))
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "account_lookup_failed");
    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM payout_methods"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn methods_can_be_made_default_and_removed_by_their_owner(pool: PgPool) -> sqlx::Result<()> {
    let app = fiat_app(pool.clone(), fake_fiat()).await;
    let owner = session_for(&pool, 11, "reviewer").await;
    let (_, created) = send(
        &app,
        "POST",
        "/v1/me/payout-methods",
        Some(&owner),
        Some(json!({
            "country": "GH", "destination_type": "mobile_money",
            "fields": { "account_number": "+233241234567", "network": "MTN" }
        })),
    )
    .await;
    assert_eq!(created["is_default"], false);
    let uri = format!("/v1/me/payout-methods/{}", created["id"].as_str().unwrap());

    let (status, body) = send(
        &app,
        "PATCH",
        &uri,
        Some(&owner),
        Some(json!({ "is_default": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_default"], true);

    let stranger = session_for(&pool, 99, "stranger").await;
    let (status, _) = send(
        &app,
        "PATCH",
        &uri,
        Some(&stranger),
        Some(json!({ "is_default": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, "DELETE", &uri, Some(&stranger), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = send(&app, "DELETE", &uri, Some(&owner), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "DELETE", &uri, Some(&owner), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn receive_status_reports_fiat_and_its_minimum(pool: PgPool) -> sqlx::Result<()> {
    let app = fiat_app(pool.clone(), fake_fiat()).await;
    let receiver = session_for(&pool, 11, "reviewer").await;
    send(
        &app,
        "POST",
        "/v1/me/payout-methods",
        Some(&receiver),
        Some(json!({
            "country": "NG", "destination_type": "bank", "is_default": true,
            "fields": { "account_number": "0123456789", "bank_code": "058" }
        })),
    )
    .await;
    let sender = session_for(&pool, 10, "maintainer").await;
    let status_of = || {
        let app = app.clone();
        let sender = sender.clone();
        async move {
            send(
                &app,
                "GET",
                "/v1/users/reviewer/receive-status",
                Some(&sender),
                None,
            )
            .await
            .1
        }
    };
    // No rate quoted yet: fiat, minimum unknown.
    assert_eq!(
        status_of().await,
        json!({ "login": "reviewer", "method": "fiat" })
    );

    // A recent quote at 100,000,000 NGN/BTC makes the 1,000 NGN minimum 1,000 sats.
    let installation = mergetip_store::installations::upsert(
        &pool,
        mergetip_core::GithubInstallationId::new(77),
        "acme",
    )
    .await
    .unwrap();
    let maintainer = users::get_by_login(&pool, "maintainer").await.unwrap();
    let mut conn = pool.acquire().await?;
    let tip = tips::insert_idempotent(
        &mut conn,
        &tips::NewTip {
            idempotency_key: "k".into(),
            installation_id: installation.id,
            sender_id: maintainer.id,
            receiver_login: "reviewer".into(),
            thread: mergetip_core::ThreadRef {
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 1,
                is_pr: true,
            },
            comment_id: 1,
            command_index: 0,
            kind: mergetip_core::TipKind::Now,
            amount: mergetip_core::Sats::new(5_000),
            note: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    tips::set_fiat_details(
        &pool,
        tip.id,
        &tips::FiatDetails {
            currency: Some("NGN".into()),
            fx_rate: Some(rust_decimal::Decimal::from_str("100000000").unwrap()),
            ..tips::FiatDetails::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        status_of().await,
        json!({ "login": "reviewer", "method": "fiat", "fiat_min_sats": 1000 })
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn tips_show_their_fiat_payout_without_the_destination(pool: PgPool) -> sqlx::Result<()> {
    let app = fiat_app(pool.clone(), fake_fiat()).await;
    let sender = session_for(&pool, 10, "maintainer").await;
    let installation = mergetip_store::installations::upsert(
        &pool,
        mergetip_core::GithubInstallationId::new(77),
        "acme",
    )
    .await
    .unwrap();
    let maintainer = users::get_by_login(&pool, "maintainer").await.unwrap();
    let mut conn = pool.acquire().await?;
    for (n, fiat) in [(1, true), (2, false)] {
        let tip = tips::insert_idempotent(
            &mut conn,
            &tips::NewTip {
                idempotency_key: format!("k{n}"),
                installation_id: installation.id,
                sender_id: maintainer.id,
                receiver_login: "reviewer".into(),
                thread: mergetip_core::ThreadRef {
                    owner: "acme".into(),
                    repo: "widgets".into(),
                    number: 1,
                    is_pr: true,
                },
                comment_id: n,
                command_index: 0,
                kind: mergetip_core::TipKind::Now,
                amount: mergetip_core::Sats::new(5_000),
                note: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        if fiat {
            tips::set_fiat_details(
                &pool,
                tip.id,
                &tips::FiatDetails {
                    currency: Some("NGN".into()),
                    amount: Some(rust_decimal::Decimal::from_str("5000.00").unwrap()),
                    fx_rate: Some(rust_decimal::Decimal::from(100_000_000)),
                    fees: Some(rust_decimal::Decimal::from_str("0.10").unwrap()),
                    quote_id: Some("QT2_1".into()),
                    payout_id: Some("po_1".into()),
                    provider_status: Some("processing".into()),
                },
            )
            .await
            .unwrap();
        }
    }
    let (status, body) = send(&app, "GET", "/v1/me/tips", Some(&sender), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["items"][0]["fiat"],
        serde_json::Value::Null,
        "Lightning tip"
    );
    assert_eq!(
        body["items"][1]["fiat"],
        json!({ "currency": "NGN", "amount": "5000.00", "rate": "100000000",
                "fees": "0.10", "status": "processing" })
    );
    assert!(
        !body.to_string().contains("po_1"),
        "provider IDs stay internal"
    );
    Ok(())
}
