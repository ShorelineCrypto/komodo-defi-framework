use crate::context::CoinsActivationContext;
use crate::platform_coin_with_tokens::{EnablePlatformCoinWithTokensError, GetPlatformBalance,
                                       InitPlatformCoinWithTokensAwaitingStatus,
                                       InitPlatformCoinWithTokensInProgressStatus, InitPlatformCoinWithTokensTask,
                                       InitPlatformCoinWithTokensTaskManagerShared,
                                       InitPlatformCoinWithTokensUserAction, InitTokensAsMmCoinsError,
                                       PlatformCoinWithTokensActivationOps, RegisterTokenInfo, TokenActivationParams,
                                       TokenActivationRequest, TokenAsMmCoinInitializer, TokenInitializer, TokenOf};
use crate::prelude::*;
use async_trait::async_trait;
use coins::hd_wallet::HDPathAccountToAddressId;
use coins::my_tx_history_v2::TxHistoryStorage;
use coins::tendermint::tendermint_tx_history_v2::tendermint_history_loop;
use coins::tendermint::{tendermint_priv_key_policy, RpcNode, TendermintActivationPolicy, TendermintCoin,
                        TendermintCommons, TendermintConf, TendermintInitError, TendermintInitErrorKind,
                        TendermintProtocolInfo, TendermintPublicKey, TendermintToken, TendermintTokenActivationParams,
                        TendermintTokenInitError, TendermintTokenProtocolInfo};
use coins::{CoinBalance, CoinProtocol, MarketCoinOps, MmCoin, MmCoinEnum, PrivKeyBuildPolicy};
use common::executor::{AbortSettings, SpawnAbortable};
use common::{true_f, Future01CompatExt};
use mm2_core::mm_ctx::MmArc;
use mm2_err_handle::prelude::*;
use mm2_number::BigDecimal;
use rpc_task::RpcTaskHandleShared;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value as Json;
use std::collections::{HashMap, HashSet};

pub type TendermintCoinTaskManagerShared = InitPlatformCoinWithTokensTaskManagerShared<TendermintCoin>;

impl TokenOf for TendermintToken {
    type PlatformCoin = TendermintCoin;
}

impl RegisterTokenInfo<TendermintToken> for TendermintCoin {
    fn register_token_info(&self, token: &TendermintToken) {
        self.add_activated_token_info(token.ticker.clone(), token.decimals, token.denom.clone())
    }
}

#[derive(Clone, Deserialize)]
pub struct TendermintActivationParams {
    nodes: Vec<RpcNode>,
    pub tokens_params: Vec<TokenActivationRequest<TendermintTokenActivationParams>>,
    #[serde(default)]
    tx_history: bool,
    #[serde(default = "true_f")]
    pub get_balances: bool,
    /// /account'/change/address_index`.
    #[serde(default)]
    pub path_to_address: HDPathAccountToAddressId,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_account_public_key")]
    with_pubkey: Option<TendermintPublicKey>,
    #[serde(default)]
    is_keplr_from_ledger: bool,
}

fn deserialize_account_public_key<'de, D>(deserializer: D) -> Result<Option<TendermintPublicKey>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Json = serde::Deserialize::deserialize(deserializer)?;

    match value {
        Json::Object(mut map) => {
            if let Some(type_) = map.remove("type") {
                if let Some(value) = map.remove("value") {
                    match type_.as_str() {
                        Some("ed25519") => {
                            let value: Vec<u8> = value
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|i| i.as_u64().unwrap() as u8)
                                .collect();
                            Ok(Some(TendermintPublicKey::from_raw_ed25519(&value).unwrap()))
                        },
                        Some("secp256k1") => {
                            let value: Vec<u8> = value
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|i| i.as_u64().unwrap() as u8)
                                .collect();
                            Ok(Some(TendermintPublicKey::from_raw_secp256k1(&value).unwrap()))
                        },
                        _ => Err(serde::de::Error::custom(
                            "Unsupported pubkey algorithm. Use one of ['ed25519', 'secp256k1']",
                        )),
                    }
                } else {
                    Err(serde::de::Error::custom("Missing field 'value'."))
                }
            } else {
                Err(serde::de::Error::custom("Missing field 'type'."))
            }
        },
        _ => Err(serde::de::Error::custom("Invalid data.")),
    }
}

impl TxHistory for TendermintActivationParams {
    fn tx_history(&self) -> bool { self.tx_history }
}

impl ActivationRequestInfo for TendermintActivationParams {
    fn is_hw_policy(&self) -> bool { false } // TODO: fix when device policy is added
}

struct TendermintTokenInitializer {
    platform_coin: TendermintCoin,
}

struct TendermintTokenInitializerErr {
    ticker: String,
    inner: TendermintTokenInitError,
}

#[async_trait]
impl TokenInitializer for TendermintTokenInitializer {
    type Token = TendermintToken;
    type TokenActivationRequest = TendermintTokenActivationParams;
    type TokenProtocol = TendermintTokenProtocolInfo;
    type InitTokensError = TendermintTokenInitializerErr;

    fn tokens_requests_from_platform_request(
        platform_request: &TendermintActivationParams,
    ) -> Vec<TokenActivationRequest<Self::TokenActivationRequest>> {
        platform_request.tokens_params.clone()
    }

    async fn enable_tokens(
        &self,
        params: Vec<TokenActivationParams<Self::TokenActivationRequest, Self::TokenProtocol>>,
    ) -> Result<Vec<Self::Token>, MmError<Self::InitTokensError>> {
        params
            .into_iter()
            .map(|param| {
                let ticker = param.ticker.clone();
                TendermintToken::new(
                    param.ticker,
                    self.platform_coin.clone(),
                    param.protocol.decimals,
                    param.protocol.denom,
                )
                .mm_err(|inner| TendermintTokenInitializerErr { ticker, inner })
            })
            .collect()
    }

    fn platform_coin(&self) -> &<Self::Token as TokenOf>::PlatformCoin { &self.platform_coin }
}

impl TryFromCoinProtocol for TendermintProtocolInfo {
    fn try_from_coin_protocol(proto: CoinProtocol) -> Result<Self, MmError<CoinProtocol>> {
        match proto {
            CoinProtocol::TENDERMINT(proto) => Ok(proto),
            other => MmError::err(other),
        }
    }
}

impl TryFromCoinProtocol for TendermintTokenProtocolInfo {
    fn try_from_coin_protocol(proto: CoinProtocol) -> Result<Self, MmError<CoinProtocol>> {
        match proto {
            CoinProtocol::TENDERMINTTOKEN(proto) => Ok(proto),
            other => MmError::err(other),
        }
    }
}

impl From<TendermintTokenInitializerErr> for InitTokensAsMmCoinsError {
    fn from(err: TendermintTokenInitializerErr) -> Self {
        match err.inner {
            TendermintTokenInitError::InvalidDenom(error) => InitTokensAsMmCoinsError::TokenProtocolParseError {
                ticker: err.ticker,
                error,
            },
            TendermintTokenInitError::MyAddressError(error) | TendermintTokenInitError::Internal(error) => {
                InitTokensAsMmCoinsError::Internal(error)
            },
            TendermintTokenInitError::CouldNotFetchBalance(error) => {
                InitTokensAsMmCoinsError::CouldNotFetchBalance(error)
            },
        }
    }
}

#[derive(Serialize, Clone)]
pub struct TendermintActivationResult {
    ticker: String,
    address: String,
    current_block: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    balance: Option<CoinBalance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_balances: Option<HashMap<String, CoinBalance>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_tickers: Option<HashSet<String>>,
}

impl CurrentBlock for TendermintActivationResult {
    fn current_block(&self) -> u64 { self.current_block }
}

impl GetPlatformBalance for TendermintActivationResult {
    fn get_platform_balance(&self) -> Option<BigDecimal> { self.balance.as_ref().map(|b| b.spendable.clone()) }
}

impl From<TendermintInitError> for EnablePlatformCoinWithTokensError {
    fn from(err: TendermintInitError) -> Self {
        EnablePlatformCoinWithTokensError::PlatformCoinCreationError {
            ticker: err.ticker,
            error: err.kind.to_string(),
        }
    }
}

#[async_trait]
impl PlatformCoinWithTokensActivationOps for TendermintCoin {
    type ActivationRequest = TendermintActivationParams;
    type PlatformProtocolInfo = TendermintProtocolInfo;
    type ActivationResult = TendermintActivationResult;
    type ActivationError = TendermintInitError;

    type InProgressStatus = InitPlatformCoinWithTokensInProgressStatus;
    type AwaitingStatus = InitPlatformCoinWithTokensAwaitingStatus;
    type UserAction = InitPlatformCoinWithTokensUserAction;

    async fn enable_platform_coin(
        ctx: MmArc,
        ticker: String,
        coin_conf: &Json,
        activation_request: Self::ActivationRequest,
        protocol_conf: Self::PlatformProtocolInfo,
    ) -> Result<Self, MmError<Self::ActivationError>> {
        if protocol_conf.decimals > 18 {
            return MmError::err(TendermintInitError {
                ticker: ticker.clone(),
                kind: TendermintInitErrorKind::InvalidProtocolData(format!(
                    "'decimals' value is too high; it must be 18 or lower but the current value is {}",
                    protocol_conf.decimals
                )),
            });
        }

        let conf = TendermintConf::try_from_json(&ticker, coin_conf)?;
        let is_keplr_from_ledger = activation_request.is_keplr_from_ledger && activation_request.with_pubkey.is_some();

        let activation_policy = if let Some(pubkey) = activation_request.with_pubkey {
            TendermintActivationPolicy::with_public_key(pubkey)
        } else {
            let private_key_policy =
                PrivKeyBuildPolicy::detect_priv_key_policy(&ctx).mm_err(|e| TendermintInitError {
                    ticker: ticker.clone(),
                    kind: TendermintInitErrorKind::Internal(e.to_string()),
                })?;

            let tendermint_private_key_policy =
                tendermint_priv_key_policy(&conf, &ticker, private_key_policy, activation_request.path_to_address)?;

            TendermintActivationPolicy::with_private_key_policy(tendermint_private_key_policy)
        };

        let coin = TendermintCoin::init(
            &ctx,
            ticker.clone(),
            conf,
            protocol_conf,
            activation_request.nodes,
            activation_request.tx_history,
            activation_policy,
            is_keplr_from_ledger,
        )
        .await?;

        Ok(coin)
    }

    async fn enable_global_nft(
        &self,
        _activation_request: &Self::ActivationRequest,
    ) -> Result<Option<MmCoinEnum>, MmError<Self::ActivationError>> {
        Ok(None)
    }

    fn try_from_mm_coin(coin: MmCoinEnum) -> Option<Self>
    where
        Self: Sized,
    {
        match coin {
            MmCoinEnum::Tendermint(coin) => Some(coin),
            _ => None,
        }
    }

    fn token_initializers(
        &self,
    ) -> Vec<Box<dyn TokenAsMmCoinInitializer<PlatformCoin = Self, ActivationRequest = Self::ActivationRequest>>> {
        vec![Box::new(TendermintTokenInitializer {
            platform_coin: self.clone(),
        })]
    }

    async fn get_activation_result(
        &self,
        _task_handle: Option<RpcTaskHandleShared<InitPlatformCoinWithTokensTask<TendermintCoin>>>,
        activation_request: &Self::ActivationRequest,
        _nft_global: &Option<MmCoinEnum>,
    ) -> Result<Self::ActivationResult, MmError<Self::ActivationError>> {
        let current_block = self.current_block().compat().await.map_to_mm(|e| TendermintInitError {
            ticker: self.ticker().to_owned(),
            kind: TendermintInitErrorKind::RpcError(e),
        })?;

        if !activation_request.get_balances {
            return Ok(TendermintActivationResult {
                ticker: self.ticker().to_owned(),
                address: self.account_id.to_string(),
                current_block,
                balance: None,
                tokens_balances: None,
                tokens_tickers: Some(
                    self.tokens_info
                        .lock()
                        .clone()
                        .into_values()
                        .map(|t| t.ticker)
                        .collect(),
                ),
            });
        }

        let balances = self.get_all_balances().await.mm_err(|e| TendermintInitError {
            ticker: self.ticker().to_owned(),
            kind: TendermintInitErrorKind::RpcError(e.to_string()),
        })?;

        Ok(TendermintActivationResult {
            address: self.account_id.to_string(),
            current_block,
            balance: Some(CoinBalance {
                spendable: balances.platform_balance,
                unspendable: BigDecimal::default(),
            }),
            tokens_balances: Some(
                balances
                    .tokens_balances
                    .into_iter()
                    .map(|(ticker, balance)| {
                        (ticker, CoinBalance {
                            spendable: balance,
                            unspendable: BigDecimal::default(),
                        })
                    })
                    .collect(),
            ),
            ticker: self.ticker().to_owned(),
            tokens_tickers: None,
        })
    }

    fn start_history_background_fetching(
        &self,
        ctx: MmArc,
        storage: impl TxHistoryStorage,
        initial_balance: Option<BigDecimal>,
    ) {
        let fut = tendermint_history_loop(self.clone(), storage, ctx, initial_balance);

        let settings = AbortSettings::info_on_abort(format!("tendermint_history_loop stopped for {}", self.ticker()));
        self.spawner().spawn_with_settings(fut, settings);
    }

    fn rpc_task_manager(activation_ctx: &CoinsActivationContext) -> &TendermintCoinTaskManagerShared {
        &activation_ctx.init_tendermint_coin_task_manager
    }
}
