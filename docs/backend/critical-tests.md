# Critical payment tests

The ten behaviours that must never regress. Each is an automated test run by `make check-backend`
and CI (`cargo test --workspace` against Postgres). Paths are under `backend/crates/`.

| # | Behaviour | Test |
| --- | --- | --- |
| 1 | A delivery received 3× (plus a second delivery of the same comment) creates one tip and one payment | `e2e/tests/tip_flow.rs::redeliveries_pay_once` |
| 2 | A worker killed after `pay_invoice` is reconciled, never paid twice | `e2e/tests/tip_flow.rs::a_worker_killed_mid_payment_never_pays_twice` (real runner, lease expiry); `engine/tests/payments.rs::a_worker_killed_mid_payment_never_pays_twice` |
| 3 | An LNURL invoice for the wrong amount is never paid | `e2e/tests/tip_flow.rs::a_wrong_amount_invoice_is_never_paid`; per-check cases in `payer/src/lnurl_tests.rs` |
| 4 | An edited comment containing `/tip` is ignored | `engine/tests/flow.rs::edits_and_bot_comments_create_nothing` |
| 5 | Self, bot and unknown receivers are rejected with the right reply | `engine/tests/flow.rs::self_bot_and_unknown_receivers_are_rejected_with_the_right_reply` |
| 6 | An over-threshold tip "confirmed" through a comment is not paid | `engine/tests/confirmations.rs::no_comment_can_confirm_a_high_value_tip`; `e2e/tests/journeys.rs::a_high_value_tip_is_paid_only_after_web_confirmation` |
| 7 | The daily cap hit mid-batch never overspends | `engine/tests/payments.rs::concurrent_payments_never_exceed_the_daily_cap` |
| 8 | A reused claim token is refused | `api/tests/claims.rs::the_receiver_claims_once_and_payment_starts`, `api/tests/claims.rs::two_claims_at_once_pay_once` |
| 9 | A payment the wallet can't confirm is never paid again | `engine/tests/payments.rs::an_unconfirmable_payment_is_never_paid_again` |
| 10 | No NWC URI, `secret=`, email, OTP, session, bearer or GitHub token appears in the logs | `e2e/tests/log_scan.rs::no_secret_ever_reaches_the_logs` (journeys A, extension token, OTP confirmation and a pledge claim, captured at `debug`); also `api/tests/log_redaction.rs`, `api/tests/payout_logs.rs` |
