//! An in-memory [`FiatProvider`] for tests: scripted answers, recorded calls.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, MutexGuard},
};

use async_trait::async_trait;
use rust_decimal::Decimal;
use time::{Duration, OffsetDateTime};

use crate::{
    AccountQuery, Beneficiary, Country, CountryRequirements, ExchangeRate, FiatError, FiatProvider,
    InitializeRequest, InitializedPayout, Limit, Payout, Quote, QuoteRequest, ResolvedAccount,
};

#[derive(Debug, Default)]
pub struct FakeState {
    pub countries: Vec<Country>,
    pub requirements: HashMap<String, CountryRequirements>,
    pub limits: Vec<Limit>,
    /// Account Lookup answers, by account number; anything else is refused.
    pub accounts: HashMap<String, ResolvedAccount>,
    /// Local currency per BTC.
    pub rate: Decimal,
    /// Invoices handed out by Initialize, in order (the test mints them for the quoted amount).
    pub invoices: VecDeque<(String, OffsetDateTime)>,
    /// This many Initialize calls answer `QuoteExpired` first.
    pub expire_initializes: u32,
    /// Recorded calls.
    pub quotes: Vec<QuoteRequest>,
    pub initialized: Vec<(String, String, Beneficiary)>,
    pub payouts: HashMap<String, Payout>,
}

#[derive(Debug, Default)]
pub struct FakeFiat {
    state: Mutex<FakeState>,
}

impl FakeFiat {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl FiatProvider for FakeFiat {
    async fn supported_countries(&self) -> Result<Vec<Country>, FiatError> {
        Ok(self.state().countries.clone())
    }

    async fn country_requirements(&self, code: &str) -> Result<CountryRequirements, FiatError> {
        self.state()
            .requirements
            .get(&code.to_ascii_uppercase())
            .cloned()
            .ok_or_else(|| FiatError::Rejected {
                status: 404,
                code: None,
                message: "unsupported country".into(),
            })
    }

    async fn limits(&self) -> Result<Vec<Limit>, FiatError> {
        Ok(self.state().limits.clone())
    }

    async fn account_lookup(&self, query: &AccountQuery) -> Result<ResolvedAccount, FiatError> {
        self.state()
            .accounts
            .get(&query.account_number)
            .cloned()
            .ok_or_else(|| FiatError::Rejected {
                status: 400,
                code: Some("VALIDATION_ERROR".into()),
                message: "account not found".into(),
            })
    }

    async fn create_quote(&self, request: &QuoteRequest) -> Result<Quote, FiatError> {
        let mut state = self.state();
        state.quotes.push(request.clone());
        let n = state.quotes.len();
        Ok(Quote {
            id: format!("q{n}"),
            quote_id: format!("QT2_{n}"),
            status: Some("quote".into()),
            amount: request.amount,
            settlement_amount: (request.amount * state.rate).round_dp(2),
            exchange_rate: ExchangeRate { rate: state.rate },
            fees: Some(Decimal::new(10, 2)),
            expires_at: OffsetDateTime::now_utc() + Duration::minutes(15),
        })
    }

    async fn initialize_payout(
        &self,
        quote_id: &str,
        request: &InitializeRequest,
    ) -> Result<InitializedPayout, FiatError> {
        let mut state = self.state();
        state.initialized.push((
            quote_id.to_owned(),
            request.reference.clone(),
            request.beneficiary.expose().clone(),
        ));
        if state.expire_initializes > 0 {
            state.expire_initializes -= 1;
            return Err(FiatError::QuoteExpired);
        }
        let (payment_request, expires_at) = state
            .invoices
            .pop_front()
            .ok_or_else(|| FiatError::Unavailable("no invoice scripted".into()))?;
        let id = format!("po_{}", state.initialized.len());
        state.payouts.insert(
            id.clone(),
            Payout {
                id: id.clone(),
                reference: Some(request.reference.clone()),
                status: "INITIATED".into(),
                settlement_amount: None,
            },
        );
        Ok(InitializedPayout {
            id,
            status: "INITIATED".into(),
            payment_request,
            expires_at,
        })
    }

    async fn get_payout(&self, id: &str) -> Result<Payout, FiatError> {
        self.state()
            .payouts
            .get(id)
            .cloned()
            .ok_or_else(|| FiatError::Rejected {
                status: 404,
                code: None,
                message: "no such payout".into(),
            })
    }
}
