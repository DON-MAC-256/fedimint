use bitcoin_hashes::Hash as BitcoinHash;
use config::ClientConfig;
use database::batch::{BatchItem, Element};
use database::{
    BatchDb, BincodeSerialized, Database, DatabaseKey, DatabaseKeyPrefix, DecodingError,
    PrefixSearchable,
};
use futures::future::JoinAll;
use mint_api::{
    Amount, Coin, CoinNonce, Coins, InvalidAmountTierError, Keys, PegInRequest, SigResponse,
    SignRequest, TransactionId, TxId,
};
use rand::seq::SliceRandom;
use rand::{CryptoRng, RngCore};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tbs::{blind_message, unblind_signature, AggregatePublicKey, BlindedMessage, BlindingKey};
use thiserror::Error;
use tracing::debug;

pub const DB_PREFIX_COIN: u8 = 0x20;
pub const DB_PREFIX_ISSUANCE: u8 = 0x21;

pub struct MintClient<D> {
    cfg: ClientConfig,
    db: D,
    http_client: reqwest::Client, // TODO: use trait object
}

/// Client side representation of one coin in an issuance request that keeps all necessary
/// information to generate one spendable coin once the blind signature arrives.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CoinRequest {
    /// Spend key from which the coin nonce (corresponding public key) is derived
    spend_key: musig::SecKey,
    /// Nonce belonging to the secret key
    nonce: CoinNonce,
    /// Key to unblind the blind signature supplied by the mint for this coin
    blinding_key: BlindingKey,
}

/// Client side representation of an issuance request that keeps all necessary information to
/// generate spendable coins once the blind signatures arrive.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IssuanceRequest {
    /// All coins in this request
    coins: Coins<CoinRequest>,
}

/// Represents a coin that can be spent by us (i.e. we can sign a transaction with the secret key
/// belonging to the nonce.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpendableCoin {
    pub coin: Coin,
    pub spend_key: musig::SecKey,
}

#[derive(Debug, Clone)]
pub struct IssuanceKey {
    issuance_id: TransactionId,
}

#[derive(Debug, Clone)]
pub struct IssuanceKeyPrefix;

#[derive(Debug, Clone)]
pub struct CoinKey {
    amount: Amount,
    nonce: CoinNonce,
}

#[derive(Debug, Clone)]
pub struct CoinKeyPrefix;

impl<D> MintClient<D>
where
    D: Database + PrefixSearchable + BatchDb + Sync,
{
    pub fn new(cfg: ClientConfig, db: D) -> Self {
        MintClient {
            cfg,
            db,
            http_client: Default::default(),
        }
    }

    pub async fn peg_in<R: RngCore + CryptoRng>(
        &self,
        peg_in_proof: Amount,
        mut rng: R,
    ) -> Result<TransactionId, ClientError> {
        // TODO: use real peg-in proof
        let amount = peg_in_proof;
        let (issuance_request, sig_req) = IssuanceRequest::new(amount, &self.cfg.mint_pk, &mut rng);
        let req = PegInRequest {
            blind_tokens: sig_req,
            proof: (),
        };

        let req_id = req.id();
        let issuance_key = IssuanceKey {
            issuance_id: req_id,
        };
        let issuance_value = BincodeSerialized::borrowed(&issuance_request);
        self.db
            .insert_entry(&issuance_key, &issuance_value)
            .expect("DB error");

        // Try all mints in random order, break early if enough could be reached
        let mut successes: usize = 0;
        for url in self
            .cfg
            .mints
            .choose_multiple(&mut rng, self.cfg.mints.len())
        {
            let res = self
                .http_client
                .put(&format!("{}/issuance/pegin", url))
                .json(&req)
                .send()
                .await
                .expect("API error");

            if res.status() == StatusCode::OK {
                successes += 1;
            }

            if successes >= 2 {
                // TODO: make this max-faulty +1
                break;
            }
        }

        if successes == 0 {
            Err(ClientError::MintError)
        } else {
            Ok(req_id)
        }
    }

    pub async fn fetch_all<R: RngCore + CryptoRng>(
        &self,
        mut rng: R,
    ) -> Result<Vec<TransactionId>, ClientError> {
        let chosen_mint = self
            .cfg
            .mints
            .choose(&mut rng)
            .expect("We need at least one mint");

        let fetched = self
            .db
            .find_by_prefix::<_, IssuanceKey, BincodeSerialized<IssuanceRequest>>(
                &IssuanceKeyPrefix,
            )
            .map(|res| {
                let (id, issuance) = res.expect("DB error");
                let id = id.issuance_id;
                let issuance = issuance.into_owned();

                async move {
                    let url = format!("{}/issuance/{}", chosen_mint, id);
                    let response = self
                        .http_client
                        .get(&url)
                        .send()
                        .await
                        .map_err(|_| ClientError::MintError);

                    let signature: SigResponse = match response {
                        Ok(response) if response.status() == StatusCode::OK => {
                            response.json().await.map_err(|_| ClientError::MintError)
                        }
                        _ => Err(ClientError::MintError),
                    }?;

                    Ok::<_, ClientError>((id, issuance.finalize(signature, &self.cfg.mint_pk)?))
                }
            })
            .collect::<JoinAll<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<(TransactionId, Coins<SpendableCoin>)>, ClientError>>()?;

        let ids = fetched.iter().map(|(id, _)| *id).collect::<Vec<_>>();

        let batch = fetched
            .into_iter()
            .flat_map(|(id, coins)| {
                coins
                    .into_iter()
                    .map(|(amount, coin): (Amount, SpendableCoin)| {
                        let key = CoinKey {
                            amount,
                            nonce: coin.coin.0.clone(),
                        };
                        let value = BincodeSerialized::owned(coin);
                        BatchItem::InsertNewElement(Element {
                            key: Box::new(key),
                            value: Box::new(value),
                        })
                    })
                    .chain(std::iter::once(BatchItem::DeleteElement(Box::new(
                        IssuanceKey { issuance_id: id },
                    ))))
            })
            .collect::<Vec<_>>();
        self.db.apply_batch(&batch).expect("DB error");

        Ok(ids)
    }

    pub fn coins(&self) -> Coins<SpendableCoin> {
        self.db
            .find_by_prefix::<_, CoinKey, BincodeSerialized<SpendableCoin>>(&CoinKeyPrefix)
            .map(|res| {
                let (key, value) = res.expect("DB error");
                (key.amount, value.into_owned())
            })
            .collect()
    }

    pub fn spend_coins(&self, coins: &Coins<SpendableCoin>) {
        let batch = coins
            .iter()
            .map(|(amount, coin)| {
                BatchItem::DeleteElement(Box::new(CoinKey {
                    amount,
                    nonce: coin.coin.0.clone(),
                }))
            })
            .collect::<Vec<_>>();

        self.db.apply_batch(&batch).expect("DB error");
    }
}

impl IssuanceRequest {
    /// Generate a new `IssuanceRequest` and the associates [`SignRequest`]
    pub fn new<K>(
        amount: Amount,
        amount_tiers: &Keys<K>,
        mut rng: impl RngCore + CryptoRng,
    ) -> (IssuanceRequest, SignRequest) {
        let (requests, blinded_nonces): (Coins<_>, Coins<_>) =
            Coins::represent_amount(amount, amount_tiers)
                .into_iter()
                .map(|(amt, ())| {
                    let (request, blind_msg) = CoinRequest::new(&mut rng);
                    ((amt, request), (amt, blind_msg))
                })
                .unzip();

        debug!(
            "Generated issuance request for {} ({} coins, tiers {:?})",
            amount,
            requests.coin_count(),
            requests.coins.keys().collect::<Vec<_>>()
        );

        let sig_req = SignRequest(blinded_nonces);
        let issuance_req = IssuanceRequest { coins: requests };

        (issuance_req, sig_req)
    }

    /// Finalize the issuance request using a [`SigResponse`] from the mint containing the blind
    /// signatures for all coins in this `IssuanceRequest`. It also takes the mint's
    /// [`AggregatePublicKey`] to validate the supplied blind signatures.
    pub fn finalize(
        &self,
        bsigs: SigResponse,
        mint_pub_key: &Keys<AggregatePublicKey>,
    ) -> Result<Coins<SpendableCoin>, CoinFinalizationError> {
        if !self.coins.structural_eq(&bsigs.0) {
            return Err(CoinFinalizationError::WrongMintAnswer);
        }

        self.coins
            .iter()
            .zip(bsigs.0)
            .enumerate()
            .map(|(idx, ((amt, coin_req), (_amt, bsig)))| {
                let sig = unblind_signature(coin_req.blinding_key, bsig);
                let coin = Coin(coin_req.nonce.clone(), sig);
                if coin.verify(*mint_pub_key.tier(&amt)?) {
                    let coin = SpendableCoin {
                        coin,
                        spend_key: coin_req.spend_key.clone(),
                    };

                    Ok((amt, coin))
                } else {
                    Err(CoinFinalizationError::InvalidSignature(idx))
                }
            })
            .collect()
    }

    pub fn coin_count(&self) -> usize {
        self.coins.coins.values().map(|v| v.len()).sum()
    }
}

impl CoinRequest {
    /// Generate a request session for a single coin and returns it plus the corresponding blinded
    /// message
    fn new(mut rng: impl RngCore + CryptoRng) -> (CoinRequest, BlindedMessage) {
        let spend_key = musig::SecKey::random(musig::rng_adapt::RngAdaptor(&mut rng));
        let nonce = CoinNonce(spend_key.to_public());

        let (blinding_key, blinded_nonce) = blind_message(nonce.to_message());

        let cr = CoinRequest {
            spend_key,
            nonce,
            blinding_key,
        };

        (cr, blinded_nonce)
    }
}

#[derive(Error, Debug)]
pub enum CoinFinalizationError {
    #[error("The returned answer does not fit the request")]
    WrongMintAnswer,
    #[error("The blind signature at index {0} is invalid")]
    InvalidSignature(usize),
    #[error("Expected signatures for issuance request {0}, got signatures for request {1}")]
    InvalidIssuanceId(TransactionId, TransactionId),
    #[error("Invalid amount tier {0:?}")]
    InvalidAmountTier(Amount),
}

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("All mints responded with an error")]
    MintError,
    #[error("Could not finalize issuance request: {0}")]
    FinalizationError(CoinFinalizationError),
}

impl From<InvalidAmountTierError> for CoinFinalizationError {
    fn from(e: InvalidAmountTierError) -> Self {
        CoinFinalizationError::InvalidAmountTier(e.0)
    }
}

impl From<CoinFinalizationError> for ClientError {
    fn from(e: CoinFinalizationError) -> Self {
        ClientError::FinalizationError(e)
    }
}

impl DatabaseKeyPrefix for IssuanceKey {
    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(33);
        bytes.push(DB_PREFIX_ISSUANCE);
        bytes.extend_from_slice(&self.issuance_id[..]);
        bytes
    }
}

impl DatabaseKey for IssuanceKey {
    fn from_bytes(data: &[u8]) -> Result<Self, DecodingError> {
        if data.len() != 33 {
            Err(DecodingError("IssuanceKey: expected 33 bytes".into()))
        } else if data[0] != DB_PREFIX_ISSUANCE {
            Err(DecodingError("IssuanceKey: wrong prefix".into()))
        } else {
            Ok(IssuanceKey {
                issuance_id: TransactionId::from_slice(&data[1..]).unwrap(),
            })
        }
    }
}

impl DatabaseKeyPrefix for IssuanceKeyPrefix {
    fn to_bytes(&self) -> Vec<u8> {
        vec![DB_PREFIX_ISSUANCE]
    }
}

impl DatabaseKeyPrefix for CoinKey {
    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(9);
        bytes.push(DB_PREFIX_COIN);
        bytes.extend_from_slice(&self.amount.milli_sat.to_be_bytes()[..]);
        bytes.extend_from_slice(&self.nonce.to_bytes());

        bytes
    }
}

impl DatabaseKey for CoinKey {
    fn from_bytes(data: &[u8]) -> Result<Self, DecodingError> {
        if data.len() < 9 {
            Err(DecodingError("CoinKey: expected at least 9 bytes".into()))
        } else if data[0] != DB_PREFIX_COIN {
            Err(DecodingError("CoinKey: wrong prefix".into()))
        } else {
            let mut amount_bytes = [0u8; 8];
            amount_bytes.copy_from_slice(&data[1..9]);
            let amount = Amount {
                milli_sat: u64::from_be_bytes(amount_bytes),
            };

            let nonce = CoinNonce::from_bytes(&data[9..]);

            Ok(CoinKey { amount, nonce })
        }
    }
}

impl DatabaseKeyPrefix for CoinKeyPrefix {
    fn to_bytes(&self) -> Vec<u8> {
        vec![DB_PREFIX_COIN]
    }
}