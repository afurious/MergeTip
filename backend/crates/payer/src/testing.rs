//! Test and local-dev helpers (feature `testing`): mint real, signed regtest invoices whose
//! preimage can be re-derived from the invoice alone, so a mock wallet can "pay" them.
//!
//! The preimage is `HMAC-SHA256(seed, payment_secret)`. The payment secret travels inside the
//! invoice, so anyone holding `seed` (the mock LNURL server and the mock wallet share it) can
//! produce a preimage that hashes to the invoice's payment hash, without any shared state.

use std::time::Duration;

use bitcoin::{
    hashes::{Hash, sha256},
    secp256k1::{Secp256k1, SecretKey},
};
use lightning_invoice::{Bolt11Invoice, InvoiceBuilder, PaymentSecret};
use mergetip_core::Msats;
use time::OffsetDateTime;

use crate::Network;

/// The node key every minted invoice is signed with. Test-only, never a real node.
const NODE_KEY: [u8; 32] = [0x42; 32];

#[derive(Debug, Clone)]
pub struct MintParams {
    pub network: Network,
    pub amount: Msats,
    /// The LNURL metadata the invoice's description hash commits to.
    pub metadata: String,
    pub created_at: OffsetDateTime,
    pub expiry: Duration,
    pub seed: [u8; 32],
    /// Random when `None`.
    pub payment_secret: Option<[u8; 32]>,
}

impl Default for MintParams {
    fn default() -> Self {
        Self {
            network: Network::Regtest,
            amount: Msats::new(1_000),
            metadata: "[]".into(),
            created_at: OffsetDateTime::now_utc(),
            expiry: Duration::from_secs(600),
            seed: [7; 32],
            payment_secret: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedInvoice {
    pub bolt11: String,
    pub payment_hash: String,
    pub preimage: String,
}

pub fn mint_invoice(params: &MintParams) -> MintedInvoice {
    let secret = params
        .payment_secret
        .unwrap_or_else(|| mergetip_crypto::random_bytes::<32>().expect("system randomness"));
    let preimage = preimage_for(&params.seed, &secret);
    let payment_hash = sha256::Hash::hash(&preimage);
    let since_epoch = Duration::from_secs(
        u64::try_from(params.created_at.unix_timestamp()).expect("timestamps after 1970"),
    );
    let key = SecretKey::from_slice(&NODE_KEY).expect("a valid test key");
    let secp = Secp256k1::new();
    let invoice = InvoiceBuilder::new(params.network.currency())
        .description_hash(sha256::Hash::hash(params.metadata.as_bytes()))
        .payment_hash(payment_hash)
        .payment_secret(PaymentSecret(secret))
        .duration_since_epoch(since_epoch)
        .min_final_cltv_expiry_delta(144)
        .amount_milli_satoshis(params.amount.get())
        .expiry_time(params.expiry)
        .build_signed(|message| secp.sign_ecdsa_recoverable(message, &key))
        .expect("a well-formed test invoice");
    MintedInvoice {
        bolt11: invoice.to_string(),
        payment_hash: hex::encode(payment_hash.to_byte_array()),
        preimage: hex::encode(preimage),
    }
}

pub fn preimage_for(seed: &[u8; 32], payment_secret: &[u8; 32]) -> [u8; 32] {
    mergetip_crypto::hmac_sha256(seed, payment_secret)
}

/// The preimage of an invoice minted with `seed`, derived from the invoice alone.
pub fn preimage_from_invoice(seed: &[u8; 32], bolt11: &str) -> Option<[u8; 32]> {
    let invoice: Bolt11Invoice = bolt11.parse().ok()?;
    Some(preimage_for(seed, &invoice.payment_secret().0))
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn minted_preimage_hashes_to_the_payment_hash() {
        let minted = mint_invoice(&MintParams::default());
        let preimage = hex::decode(&minted.preimage).unwrap();
        assert_eq!(hex::encode(Sha256::digest(&preimage)), minted.payment_hash);
        let derived = preimage_from_invoice(&[7; 32], &minted.bolt11).unwrap();
        assert_eq!(hex::encode(derived), minted.preimage);
        assert_ne!(
            preimage_from_invoice(&[8; 32], &minted.bolt11).map(hex::encode),
            Some(minted.preimage),
            "another seed derives another preimage"
        );
    }
}

pub use mock_wallet::{MockWallet, MockWalletOptions, behaviour};

mod mock_wallet {
    //! A NIP-47 wallet on a relay, for tests and local development. It pays invoices minted
    //! with its `seed` by deriving their preimage; certain amounts trigger failure modes (see
    //! [`behaviour`]).

    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use bitcoin::hashes::{Hash, sha256};
    use lightning_invoice::Bolt11Invoice;
    use nostr::nips::nip47::{
        ErrorCode, GetInfoResponse, LookupInvoiceResponse, Method, NIP47Error, Nip47Ciphers,
        Nip47Tag, NostrWalletConnectUri, PayInvoiceResponse, Request, RequestParams, Response,
        ResponseResult, TransactionState, TransactionType,
    };
    use nostr_sdk::prelude::{
        Client, ClientNotification, Event, EventBuilder, Filter, FinalizeEvent, Keys, Kind,
        RelayUrl, StreamExt, Tag, Timestamp,
    };
    use tokio::task::JoinHandle;

    use super::preimage_from_invoice;
    use crate::nwc::PayError;

    /// Amounts (in sats) that make the mock misbehave, so every failure path can be exercised.
    pub mod behaviour {
        /// Answers `INSUFFICIENT_BALANCE`.
        pub const INSUFFICIENT_BALANCE_SATS: u64 = 666;
        /// Pays, but the answer is "lost": the client times out, and `lookup_invoice` later
        /// reports the payment as settled. Models a crash or relay drop after paying.
        pub const PAYS_SILENTLY_SATS: u64 = 777;
        /// Answers `QUOTA_EXCEEDED`.
        pub const QUOTA_EXCEEDED_SATS: u64 = 888;
        /// Never pays and never answers: `lookup_invoice` says not found.
        pub const NEVER_ANSWERS_SATS: u64 = 999;
    }

    const METHODS: [&str; 3] = ["pay_invoice", "get_info", "lookup_invoice"];

    pub struct MockWallet {
        uri: String,
        paid: Arc<Mutex<HashMap<String, String>>>,
        client: Client,
        task: JoinHandle<()>,
    }

    impl std::fmt::Debug for MockWallet {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockWallet").finish_non_exhaustive()
        }
    }

    /// How a [`MockWallet`] runs. Fixed keys keep its URI stable across restarts (local dev).
    #[derive(Debug, Clone)]
    pub struct MockWalletOptions {
        /// Where the wallet connects.
        pub relay: String,
        /// The relay written into the URI, if clients reach it at another address (e.g. the
        /// wallet runs in Docker Compose and clients on the host).
        pub advertised_relay: Option<String>,
        pub seed: [u8; 32],
        pub wallet_secret: Option<[u8; 32]>,
        pub app_secret: Option<[u8; 32]>,
    }

    impl MockWallet {
        /// Publishes the wallet's info event on `relay_url` and starts answering requests.
        pub async fn start(relay_url: &str, seed: [u8; 32]) -> Result<Self, PayError> {
            Self::start_with(MockWalletOptions {
                relay: relay_url.to_owned(),
                advertised_relay: None,
                seed,
                wallet_secret: None,
                app_secret: None,
            })
            .await
        }

        pub async fn start_with(options: MockWalletOptions) -> Result<Self, PayError> {
            let parse =
                |url: &str| RelayUrl::parse(url).map_err(|err| PayError::Relay(err.to_string()));
            let relay = parse(&options.relay)?;
            let advertised = match &options.advertised_relay {
                Some(url) => parse(url)?,
                None => relay.clone(),
            };
            let keys = |secret: Option<[u8; 32]>| match secret {
                Some(bytes) => nostr_sdk::prelude::SecretKey::from_slice(&bytes)
                    .map(Keys::new)
                    .map_err(|err| PayError::Protocol(err.to_string())),
                None => Ok(Keys::generate()),
            };
            let wallet = keys(options.wallet_secret)?;
            let app = keys(options.app_secret)?;
            let seed = options.seed;
            let client = Client::default();
            client
                .add_relay(&relay)
                .await
                .map_err(|err| PayError::Relay(err.to_string()))?;
            client
                .try_connect()
                .timeout(std::time::Duration::from_secs(10))
                .await;

            let encryption = Tag::parse(["encryption", "nip44_v2 nip04"])
                .map_err(|err| PayError::Protocol(err.to_string()))?;
            let info = EventBuilder::new(Kind::WalletConnectInfo, METHODS.join(" "))
                .tag(encryption)
                .finalize(&wallet)
                .map_err(|err| PayError::Protocol(err.to_string()))?;
            client
                .send_event(&info)
                .await
                .map_err(|err| PayError::Relay(err.to_string()))?;

            let filter = Filter::new()
                .kind(Kind::WalletConnectRequest)
                .pubkey(wallet.public_key())
                .since(Timestamp::now());
            client
                .subscribe(filter)
                .await
                .map_err(|err| PayError::Relay(err.to_string()))?;

            let paid = Arc::new(Mutex::new(HashMap::new()));
            let mut notifications = client.notifications();
            let task_client = client.clone();
            let task_paid = paid.clone();
            let task_wallet = wallet.clone();
            let task = tokio::spawn(async move {
                while let Some(notification) = notifications.next().await {
                    if let ClientNotification::Event { event, .. } = notification
                        && event.kind == Kind::WalletConnectRequest
                    {
                        answer(&task_client, &task_wallet, &event, &seed, &task_paid).await;
                    }
                }
            });

            let uri = NostrWalletConnectUri::new(
                wallet.public_key(),
                vec![advertised],
                app.secret_key().clone(),
                None,
            );
            Ok(Self {
                uri: uri.to_string(),
                paid,
                client,
                task,
            })
        }

        /// The `nostr+walletconnect://` URI a sender would paste.
        pub fn uri(&self) -> &str {
            &self.uri
        }

        /// Payment hashes paid so far.
        pub fn paid_hashes(&self) -> Vec<String> {
            self.paid
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .keys()
                .cloned()
                .collect()
        }

        pub async fn stop(self) {
            self.task.abort();
            self.client.shutdown().await;
        }
    }

    async fn answer(
        client: &Client,
        wallet: &Keys,
        event: &Event,
        seed: &[u8; 32],
        paid: &Mutex<HashMap<String, String>>,
    ) {
        let cipher = if event.tags.iter().any(|t| {
            matches!(Nip47Tag::try_from(t), Ok(Nip47Tag::Encryption(c)) if c.has(Nip47Ciphers::NIP44V2))
        }) {
            Nip47Ciphers::NIP44V2
        } else {
            Nip47Ciphers::NIP04
        };
        let Ok(plain) = cipher.decrypt(wallet.secret_key(), &event.pubkey, &event.content) else {
            return;
        };
        let Ok(request) = Request::from_json(&plain) else {
            return;
        };
        let Some(response) = respond(request, seed, paid) else {
            return;
        };
        let Ok(json) = serde_json::to_string(&response) else {
            return;
        };
        let Ok(encrypted) = cipher.encrypt(wallet.secret_key(), &event.pubkey, &json) else {
            return;
        };
        let reply = EventBuilder::new(Kind::WalletConnectResponse, encrypted)
            .tag(Tag::public_key(event.pubkey))
            .tag(Tag::event(event.id))
            .finalize(wallet);
        if let Ok(reply) = reply {
            let _ = client.send_event(&reply).await;
        }
    }

    fn error(method: Method, code: ErrorCode, message: &str) -> Response {
        Response {
            result_type: method,
            error: Some(NIP47Error {
                code,
                message: message.to_owned(),
            }),
            result: None,
        }
    }

    /// `None` means "don't answer".
    fn respond(
        request: Request,
        seed: &[u8; 32],
        paid: &Mutex<HashMap<String, String>>,
    ) -> Option<Response> {
        match request.params {
            RequestParams::GetInfo => Some(Response {
                result_type: Method::GetInfo,
                error: None,
                result: Some(ResponseResult::GetInfo(GetInfoResponse {
                    alias: Some("mergetip-mock".into()),
                    color: None,
                    pubkey: None,
                    network: Some("regtest".into()),
                    block_height: None,
                    block_hash: None,
                    methods: METHODS.iter().filter_map(|m| m.parse().ok()).collect(),
                    notifications: Vec::new(),
                })),
            }),
            RequestParams::PayInvoice(pay) => {
                let invoice: Bolt11Invoice = match pay.invoice.parse() {
                    Ok(invoice) => invoice,
                    Err(_) => {
                        return Some(error(Method::PayInvoice, ErrorCode::Other, "bad invoice"));
                    }
                };
                let sats = invoice.amount_milli_satoshis().unwrap_or(0) / 1_000;
                let hash = hex::encode(invoice.payment_hash().to_byte_array());
                match sats {
                    behaviour::INSUFFICIENT_BALANCE_SATS => Some(error(
                        Method::PayInvoice,
                        ErrorCode::InsufficientBalance,
                        "not enough sats",
                    )),
                    behaviour::QUOTA_EXCEEDED_SATS => Some(error(
                        Method::PayInvoice,
                        ErrorCode::QuotaExceeded,
                        "budget used up",
                    )),
                    behaviour::NEVER_ANSWERS_SATS => None,
                    _ => {
                        let Some(preimage) = preimage_from_invoice(seed, &pay.invoice) else {
                            return Some(error(
                                Method::PayInvoice,
                                ErrorCode::Other,
                                "unknown invoice",
                            ));
                        };
                        if sha256::Hash::hash(&preimage).to_byte_array()
                            != invoice.payment_hash().to_byte_array()
                        {
                            return Some(error(
                                Method::PayInvoice,
                                ErrorCode::PaymentFailed,
                                "no route",
                            ));
                        }
                        let preimage = hex::encode(preimage);
                        paid.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(hash, preimage.clone());
                        if sats == behaviour::PAYS_SILENTLY_SATS {
                            return None;
                        }
                        Some(Response {
                            result_type: Method::PayInvoice,
                            error: None,
                            result: Some(ResponseResult::PayInvoice(PayInvoiceResponse {
                                preimage,
                                fees_paid: Some(0),
                            })),
                        })
                    }
                }
            }
            RequestParams::LookupInvoice(lookup) => {
                let hash = lookup.payment_hash.unwrap_or_default();
                let preimage = paid
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&hash)
                    .cloned();
                Some(match preimage {
                    Some(preimage) => Response {
                        result_type: Method::LookupInvoice,
                        error: None,
                        result: Some(ResponseResult::LookupInvoice(LookupInvoiceResponse {
                            transaction_type: Some(TransactionType::Outgoing),
                            state: Some(TransactionState::Settled),
                            invoice: None,
                            description: None,
                            description_hash: None,
                            preimage: Some(preimage),
                            payment_hash: hash,
                            amount: 0,
                            fees_paid: 0,
                            created_at: Timestamp::now(),
                            expires_at: None,
                            settled_at: Some(Timestamp::now()),
                            metadata: None,
                        })),
                    },
                    None => error(
                        Method::LookupInvoice,
                        ErrorCode::NotFound,
                        "unknown payment",
                    ),
                })
            }
            _ => Some(error(
                Method::GetInfo,
                ErrorCode::NotImplemented,
                "not supported",
            )),
        }
    }
}
