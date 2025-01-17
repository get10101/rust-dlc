//! #Manager a component to create and update DLCs.

use super::{Blockchain, Oracle, Storage, Time, Wallet};
use crate::chain_monitor::{ChainMonitor, ChannelInfo, RevokedTxType, TxType};
use crate::channel::offered_channel::OfferedChannel;
use crate::channel::signed_channel::{SignedChannel, SignedChannelState, SignedChannelStateType};
use crate::channel::{Channel, ClosedChannel, ClosedPunishedChannel, SettledClosingChannel};
use crate::channel_updater::{get_unix_time_now, verify_signed_channel};
use crate::channel_updater::{self, get_signed_channel_state};
use crate::contract::{
    accepted_contract::AcceptedContract, contract_info::ContractInfo,
    contract_input::ContractInput, contract_input::OracleInput, offered_contract::OfferedContract,
    signed_contract::SignedContract, AdaptorInfo, ClosedContract, Contract, FailedAcceptContract,
    FailedSignContract, PreClosedContract,
};
use crate::contract_updater::{accept_contract, verify_accepted_and_sign_contract};
use crate::error::Error;
use crate::sub_channel_manager::get_sub_channel_in_state;
use crate::subchannel::{ClosingSubChannel, SubChannel, SubChannelState};
use crate::utils::get_object_in_state;
use crate::{ContractId, DlcChannelId, ReferenceId, Signer};
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::{Address, OutPoint, Txid};
use bitcoin::Transaction;
use bitcoin::hashes::hex::ToHex;
use dlc::FeeConfig;
use dlc_messages::channel::{
    AcceptChannel, CollaborativeCloseOffer, OfferChannel, Reject, RenewAccept, RenewConfirm,
    RenewFinalize, RenewOffer, RenewRevoke, SettleAccept, SettleConfirm, SettleFinalize,
    SettleOffer, SignChannel,
};
use dlc_messages::oracle_msgs::{OracleAnnouncement, OracleAttestation};
use dlc_messages::{
    AcceptDlc, ChannelMessage, Message as DlcMessage, OfferDlc, OnChainMessage, SignDlc,
};
use lightning::chain::chaininterface::{FeeEstimator, ConfirmationTarget};
use lightning::ln::chan_utils::{
    build_commitment_secret, derive_private_key, derive_private_revocation_key,
};
use log::{error, warn};
use secp256k1_zkp::{ecdsa::Signature, All, PublicKey, Secp256k1, SecretKey};
use secp256k1_zkp::{EcdsaAdaptorSignature, XOnlyPublicKey};
use std::collections::HashMap;
use std::ops::Deref;
use std::string::ToString;
use std::sync::Mutex;
use bitcoin::consensus::Decodable;
use bitcoin::hashes::Hash;

/// The number of confirmations required before moving a [`Contract`] to the confirmed state.
pub const NB_CONFIRMATIONS: u32 = 0;
/// The delay to set the refund value to.
pub const REFUND_DELAY: u32 = 86400 * 7;
/// The nSequence value used for CETs in DLC channels
pub const CET_NSEQUENCE: u32 = 288;
/// Timeout in seconds when waiting for a peer's reply, after which a DLC channel
/// is forced closed.
pub const PEER_TIMEOUT: u64 = 3600;

type ClosableContractInfo<'a> = Option<(
    &'a ContractInfo,
    &'a AdaptorInfo,
    Vec<(usize, OracleAttestation)>,
)>;

/// Used to create and update DLCs.
pub struct Manager<W: Deref, B: Deref, S: Deref, O: Deref, T: Deref, F: Deref>
where
    W::Target: Wallet,
    B::Target: Blockchain,
    S::Target: Storage,
    O::Target: Oracle,
    T::Target: Time,
    F::Target: FeeEstimator,
{
    oracles: HashMap<XOnlyPublicKey, O>,
    wallet: W,
    blockchain: B,
    store: S,
    secp: Secp256k1<All>,
    chain_monitor: Mutex<ChainMonitor>,
    time: T,
    fee_estimator: F,
}

macro_rules! get_contract_in_state {
    ($manager: expr, $contract_id: expr, $state: ident, $peer_id: expr) => {{
        get_object_in_state!(
            $manager,
            $contract_id,
            $state,
            $peer_id,
            Contract,
            get_contract
        )
    }};
}

pub(crate) use get_contract_in_state;

macro_rules! get_channel_in_state {
    ($manager: expr, $channel_id: expr, $state: ident, $peer_id: expr) => {{
        get_object_in_state!(
            $manager,
            $channel_id,
            $state,
            $peer_id,
            Channel,
            get_channel
        )
    }};
}

pub(crate) use get_channel_in_state;

macro_rules! get_signed_channel_rollback_state {
    ($signed_channel: ident, $state: ident, $($field: ident),*) => {{
       match $signed_channel.roll_back_state.as_ref() {
           Some(SignedChannelState::$state{$($field,)* ..}) => Ok(($($field,)*)),
           _ => Err(Error::InvalidState(format!("Expected rollback state {} got {:?}", stringify!($state), $signed_channel.state))),
        }
    }};
}

macro_rules! check_for_timed_out_channels {
    ($manager: ident, $state: ident) => {
        let channels = $manager
            .store
            .get_signed_channels(Some(SignedChannelStateType::$state))?;

        for channel in channels {
            if let SignedChannelState::$state { timeout, .. } = channel.state {
                let is_timed_out = timeout < $manager.time.unix_time_now();
                if is_timed_out {
                    let _sub_channel: Option<SubChannel> = if channel.is_sub_channel() {
                        log::info!(
                            "Skipping force-closure of subchannel {}: not supported",
                            bitcoin::hashes::hex::ToHex::to_hex(&channel.channel_id[..])
                        );
                        continue;

                        // TODO: Implement subchannel force closing
                        // let s = get_sub_channel_in_state!(
                        //     $manager,
                        //     channel.channel_id,
                        //     Signed,
                        //     None::<PublicKey>
                        // )?;
                        // Some(s)
                    } else {
                        None
                    };

                    log::warn!(
                            "Dlc channel timed out in State {:?}. Skipping force-closure of dlc channel {} while in beta.",
                            channel.state,
                            bitcoin::hashes::hex::ToHex::to_hex(&channel.channel_id[..])
                        );

                    // TODO(holzeis): Enable that logic once out of beta!
                    // log::warn!("Force closing channel that timed out. {} < {}", timeout, $manager.time.unix_time_now());
                    // match $manager.force_close_channel_internal(channel, sub_channel, true) {
                    //     Err(e) => error!("Error force closing channel {}", e),
                    //     _ => {}
                    // }
                }
            }
        }
    };
}

impl<W: Deref, B: Deref, S: Deref, O: Deref, T: Deref, F: Deref> Manager<W, B, S, O, T, F>
where
    W::Target: Wallet,
    B::Target: Blockchain,
    S::Target: Storage,
    O::Target: Oracle,
    T::Target: Time,
    F::Target: FeeEstimator,
{
    /// Create a new Manager struct.
    pub fn new(
        wallet: W,
        blockchain: B,
        store: S,
        oracles: HashMap<XOnlyPublicKey, O>,
        time: T,
        fee_estimator: F,
    ) -> Result<Self, Error> {
        let chain_monitor = store
            .get_chain_monitor()?
            .unwrap_or(ChainMonitor::new(blockchain.get_blockchain_height()?));

        Ok(Manager {
            secp: secp256k1_zkp::Secp256k1::new(),
            wallet,
            store,
            oracles,
            time,
            fee_estimator,
            chain_monitor: Mutex::new(chain_monitor),
            blockchain,
        })
    }

    /// Get the store from the Manager to access contracts.
    pub fn get_store(&self) -> &S {
        &self.store
    }

    pub(crate) fn get_wallet(&self) -> &W {
        &self.wallet
    }

    pub(crate) fn get_blockchain(&self) -> &B {
        &self.blockchain
    }

    pub(crate) fn get_time(&self) -> &T {
        &self.time
    }

    pub(crate) fn get_fee_estimator(&self) -> &F {
        &self.fee_estimator
    }

    pub(crate) fn get_secp(&self) -> &Secp256k1<All> {
        &self.secp
    }

    /// Return the chain monitor used to watch for relevant transactions on chain.
    pub fn get_chain_monitor(&self) -> &Mutex<ChainMonitor> {
        &self.chain_monitor
    }

    /// Function called to pass a DlcMessage to the Manager.
    pub fn on_dlc_message(
        &self,
        msg: &DlcMessage,
        counter_party: PublicKey,
    ) -> Result<Option<DlcMessage>, Error> {
        match msg {
            DlcMessage::OnChain(on_chain) => match on_chain {
                OnChainMessage::Offer(o) => {
                    self.on_offer_message(o, counter_party)?;
                    Ok(None)
                }
                OnChainMessage::Accept(a) => Ok(Some(self.on_accept_message(a, &counter_party)?)),
                OnChainMessage::Sign(s) => {
                    self.on_sign_message(s, &counter_party)?;
                    Ok(None)
                }
            },
            DlcMessage::Channel(channel) => match channel {
                ChannelMessage::Offer(o) => {
                    self.on_offer_channel(o, counter_party)?;
                    Ok(None)
                }
                ChannelMessage::Accept(a) => Ok(Some(DlcMessage::Channel(ChannelMessage::Sign(
                    self.on_accept_channel(a, &counter_party)?,
                )))),
                ChannelMessage::Sign(s) => {
                    self.on_sign_channel(s, &counter_party)?;
                    Ok(None)
                }
                ChannelMessage::SettleOffer(s) => match self.on_settle_offer(s, &counter_party)? {
                    Some(msg) => Ok(Some(DlcMessage::Channel(ChannelMessage::Reject(msg)))),
                    None => Ok(None),
                },
                ChannelMessage::SettleAccept(s) => Ok(Some(DlcMessage::Channel(
                    ChannelMessage::SettleConfirm(self.on_settle_accept(s, &counter_party)?),
                ))),
                ChannelMessage::SettleConfirm(s) => Ok(Some(DlcMessage::Channel(
                    ChannelMessage::SettleFinalize(self.on_settle_confirm(s, &counter_party)?),
                ))),
                ChannelMessage::SettleFinalize(s) => {
                    self.on_settle_finalize(s, &counter_party)?;
                    Ok(None)
                }
                ChannelMessage::RenewOffer(r) => match self.on_renew_offer(r, &counter_party)? {
                    Some(msg) => Ok(Some(DlcMessage::Channel(ChannelMessage::Reject(msg)))),
                    None => Ok(None),
                },
                ChannelMessage::RenewAccept(r) => Ok(Some(DlcMessage::Channel(
                    ChannelMessage::RenewConfirm(self.on_renew_accept(r, &counter_party)?),
                ))),
                ChannelMessage::RenewConfirm(r) => Ok(Some(DlcMessage::Channel(
                    ChannelMessage::RenewFinalize(self.on_renew_confirm(r, &counter_party)?),
                ))),
                ChannelMessage::RenewFinalize(r) => {
                    let revoke = self.on_renew_finalize(r, &counter_party)?;
                    Ok(Some(DlcMessage::Channel(ChannelMessage::RenewRevoke(
                        revoke,
                    ))))
                }
                ChannelMessage::CollaborativeCloseOffer(c) => {
                    self.on_collaborative_close_offer(c, &counter_party)?;
                    Ok(None)
                }
                ChannelMessage::Reject(r) => {
                    self.on_reject(r ,&counter_party)?;
                    Ok(None)
                }
                ChannelMessage::RenewRevoke(r) => {
                    self.on_renew_revoke(r, &counter_party)?;
                    Ok(None)
                }
            },
            DlcMessage::SubChannel(_) => Err(Error::InvalidParameters(
                "SubChannel messages not supported".to_string(),
            )),
        }
    }

    /// Function called to create a new DLC. The offered contract will be stored
    /// and an OfferDlc message returned.
    pub fn send_offer(
        &self,
        contract_input: &ContractInput,
        counter_party: PublicKey,
    ) -> Result<OfferDlc, Error> {
        contract_input.validate()?;

        let oracle_announcements = contract_input
            .contract_infos
            .iter()
            .map(|x| self.get_oracle_announcements(&x.oracles))
            .collect::<Result<Vec<_>, Error>>()?;

        let (offered_contract, offer_msg) = crate::contract_updater::offer_contract(
            &self.secp,
            contract_input,
            oracle_announcements,
            REFUND_DELAY,
            &counter_party,
            &self.wallet,
            &self.blockchain,
            &self.time,
        )?;

        offered_contract.validate()?;

        self.store.create_contract(&offered_contract)?;

        Ok(offer_msg)
    }

    /// Function to call to accept a DLC for which an offer was received.
    pub fn accept_contract_offer(
        &self,
        contract_id: &ContractId,
    ) -> Result<(ContractId, PublicKey, AcceptDlc), Error> {
        let offered_contract =
            get_contract_in_state!(self, contract_id, Offered, None as Option<PublicKey>)?;

        let counter_party = offered_contract.counter_party;

        let (accepted_contract, accept_msg) = accept_contract(
            &self.secp,
            &offered_contract,
            &self.wallet,
            &self.blockchain,
        )?;

        self.wallet.import_address(&Address::p2wsh(
            &accepted_contract.dlc_transactions.funding_script_pubkey,
            self.blockchain.get_network()?,
        ))?;

        let contract_id = accepted_contract.get_contract_id();

        self.store
            .update_contract(&Contract::Accepted(accepted_contract))?;

        Ok((contract_id, counter_party, accept_msg))
    }

    /// Function to call to check the state of the currently executing DLCs and
    /// update them if possible.
    pub fn periodic_check(&self) -> Result<(), Error> {
        self.check_transaction_confirmations();
        self.check_signed_contracts()?;
        self.check_confirmed_contracts()?;
        self.check_preclosed_contracts()?;
        self.channel_checks()?;

        Ok(())
    }

    fn check_transaction_confirmations(&self) {
        let blockchain = &self.blockchain;

        let (txs, txos) = {
            let chain_monitor = self.chain_monitor.lock().unwrap();
            let txs = chain_monitor.get_watched_txs();
            let txos = chain_monitor.get_watched_txos();

            (txs, txos)
        };

        for txid in txs {
            let confirmations = match blockchain.get_transaction_confirmations(&txid) {
                Ok(confirmations) => confirmations,
                Err(e) => {
                    log::error!("Failed to get transaction confirmations for {txid}: {e}");
                    continue;
                }
            };

            if confirmations > 0 {
                let tx = match blockchain.get_transaction(&txid) {
                    Ok(tx) => tx,
                    Err(e) => {
                        log::error!("Failed to get transaction for {txid}: {e}");
                        continue;
                    }
                };

                let mut chain_monitor = self.chain_monitor.lock().unwrap();
                chain_monitor.confirm_tx(tx);
            }
        }

        for txo in txos {
            let (confirmations, txid) = match blockchain.get_txo_confirmations(&txo) {
                Ok(Some((confirmations, txid))) => (confirmations, txid),
                Ok(None) => continue,
                Err(e) => {
                    log::error!("Failed to get transaction confirmations for {txo}: {e}");
                    continue;
                }
            };

            if confirmations > 0 {
                let tx = match blockchain.get_transaction(&txid) {
                    Ok(tx) => tx,
                    Err(e) => {
                        log::error!("Failed to get transaction for {txid}: {e}");
                        continue;
                    }
                };

                let mut chain_monitor = self.chain_monitor.lock().unwrap();
                chain_monitor.confirm_txo(&txo, tx.clone());
            }
        }
    }

    fn on_offer_message(
        &self,
        offered_message: &OfferDlc,
        counter_party: PublicKey,
    ) -> Result<(), Error> {
        offered_message.validate(&self.secp, REFUND_DELAY, REFUND_DELAY * 2)?;
        let contract: OfferedContract =
            OfferedContract::try_from_offer_dlc(offered_message, counter_party)?;
        contract.validate()?;

        if self.store.get_contract(&contract.id)?.is_some() {
            return Err(Error::InvalidParameters(
                "Contract with identical id already exists".to_string(),
            ));
        }

        self.store.create_contract(&contract)?;

        Ok(())
    }

    fn on_accept_message(
        &self,
        accept_msg: &AcceptDlc,
        counter_party: &PublicKey,
    ) -> Result<DlcMessage, Error> {
        let offered_contract = get_contract_in_state!(
            self,
            &accept_msg.temporary_contract_id,
            Offered,
            Some(*counter_party)
        )?;

        let (signed_contract, signed_msg) = match verify_accepted_and_sign_contract(
            &self.secp,
            &offered_contract,
            accept_msg,
            &self.wallet,
        ) {
            Ok(contract) => contract,
            Err(e) => return self.accept_fail_on_error(offered_contract, accept_msg.clone(), e),
        };

        self.wallet.import_address(&Address::p2wsh(
            &signed_contract
                .accepted_contract
                .dlc_transactions
                .funding_script_pubkey,
            self.blockchain.get_network()?,
        ))?;

        self.store
            .update_contract(&Contract::Signed(signed_contract))?;

        Ok(DlcMessage::OnChain(OnChainMessage::Sign(signed_msg)))
    }

    fn on_sign_message(&self, sign_message: &SignDlc, peer_id: &PublicKey) -> Result<(), Error> {
        let accepted_contract =
            get_contract_in_state!(self, &sign_message.contract_id, Accepted, Some(*peer_id))?;

        let (signed_contract, fund_tx) = match crate::contract_updater::verify_signed_contract(
            &self.secp,
            &accepted_contract,
            sign_message,
            &self.wallet,
        ) {
            Ok(contract) => contract,
            Err(e) => return self.sign_fail_on_error(accepted_contract, sign_message.clone(), e),
        };

        self.store
            .update_contract(&Contract::Signed(signed_contract))?;

        self.blockchain.send_transaction(&fund_tx)?;

        Ok(())
    }

    fn get_oracle_announcements(
        &self,
        oracle_inputs: &OracleInput,
    ) -> Result<Vec<OracleAnnouncement>, Error> {
        let mut announcements = Vec::new();
        for pubkey in &oracle_inputs.public_keys {
            let oracle = self
                .oracles
                .get(pubkey)
                .ok_or_else(|| Error::InvalidParameters("Unknown oracle public key".to_string()))?;
            announcements.push(oracle.get_announcement(&oracle_inputs.event_id)?.clone());
        }

        Ok(announcements)
    }

    fn sign_fail_on_error<R>(
        &self,
        accepted_contract: AcceptedContract,
        sign_message: SignDlc,
        e: Error,
    ) -> Result<R, Error> {
        error!("Error in on_sign {}", e);
        self.store
            .update_contract(&Contract::FailedSign(FailedSignContract {
                accepted_contract,
                sign_message,
                error_message: e.to_string(),
            }))?;
        Err(e)
    }

    fn accept_fail_on_error<R>(
        &self,
        offered_contract: OfferedContract,
        accept_message: AcceptDlc,
        e: Error,
    ) -> Result<R, Error> {
        error!("Error in on_accept {}", e);
        self.store
            .update_contract(&Contract::FailedAccept(FailedAcceptContract {
                offered_contract,
                accept_message,
                error_message: e.to_string(),
            }))?;
        Err(e)
    }

    fn check_signed_contract(&self, contract: &SignedContract) -> Result<(), Error> {
        let confirmations = self.blockchain.get_transaction_confirmations(
            &contract.accepted_contract.dlc_transactions.fund.txid(),
        )?;
        #[allow(clippy::absurd_extreme_comparisons)]
        if confirmations >= NB_CONFIRMATIONS {
            self.store
                .update_contract(&Contract::Confirmed(contract.clone()))?;
        }
        Ok(())
    }

    fn check_signed_contracts(&self) -> Result<(), Error> {
        for c in self.store.get_signed_contracts()? {
            if let Err(e) = self.check_signed_contract(&c) {
                error!(
                    "Error checking confirmed contract {}: {}",
                    c.accepted_contract.get_contract_id_string(),
                    e
                )
            }
        }

        Ok(())
    }

    fn check_confirmed_contracts(&self) -> Result<(), Error> {
        for c in self.store.get_confirmed_contracts()? {
            // Confirmed contracts from channel are processed in channel specific methods.
            if c.channel_id.is_some() {
                continue;
            }
            if let Err(e) = self.check_confirmed_contract(&c) {
                error!(
                    "Error checking confirmed contract {}: {}",
                    c.accepted_contract.get_contract_id_string(),
                    e
                )
            }
        }

        Ok(())
    }

    fn get_closable_contract_info<'a>(
        &'a self,
        contract: &'a SignedContract,
    ) -> ClosableContractInfo<'a> {
        let contract_infos = &contract.accepted_contract.offered_contract.contract_info;
        let adaptor_infos = &contract.accepted_contract.adaptor_infos;
        for (contract_info, adaptor_info) in contract_infos.iter().zip(adaptor_infos.iter()) {
            let matured: Vec<_> = contract_info
                .oracle_announcements
                .iter()
                .filter(|x| {
                    (x.oracle_event.event_maturity_epoch as u64) <= self.time.unix_time_now()
                })
                .enumerate()
                .collect();
            if matured.len() >= contract_info.threshold {
                let attestations: Vec<_> = matured
                    .iter()
                    .filter_map(|(i, announcement)| {
                        let oracle = self.oracles.get(&announcement.oracle_public_key)?;
                        Some((
                            *i,
                            oracle
                                .get_attestation(&announcement.oracle_event.event_id)
                                .ok()?,
                        ))
                    })
                    .collect();
                if attestations.len() >= contract_info.threshold {
                    return Some((contract_info, adaptor_info, attestations));
                }
            }
        }
        None
    }

    fn check_confirmed_contract(&self, contract: &SignedContract) -> Result<(), Error> {
        let closable_contract_info = self.get_closable_contract_info(contract);
        if let Some((contract_info, adaptor_info, attestations)) = closable_contract_info {
            let cet = crate::contract_updater::get_signed_cet(
                &self.secp,
                contract,
                contract_info,
                adaptor_info,
                &attestations,
                &self.wallet,
            )?;
            match self.close_contract(
                contract,
                cet,
                attestations.iter().map(|x| x.1.clone()).collect(),
            ) {
                Ok(closed_contract) => {
                    self.store.update_contract(&closed_contract)?;
                    return Ok(());
                }
                Err(e) => {
                    warn!(
                        "Failed to close contract {}: {}",
                        contract.accepted_contract.get_contract_id_string(),
                        e
                    );
                    return Err(e);
                }
            }
        }

        self.check_refund(contract)?;

        Ok(())
    }

    fn check_preclosed_contracts(&self) -> Result<(), Error> {
        for c in self.store.get_preclosed_contracts()? {
            if let Err(e) = self.check_preclosed_contract(&c) {
                error!(
                    "Error checking pre-closed contract {}: {}",
                    c.signed_contract.accepted_contract.get_contract_id_string(),
                    e
                )
            }
        }

        Ok(())
    }

    fn check_preclosed_contract(&self, contract: &PreClosedContract) -> Result<(), Error> {
        let broadcasted_txid = contract.signed_cet.txid();
        let confirmations = self
            .blockchain
            .get_transaction_confirmations(&broadcasted_txid)?;
        #[allow(clippy::absurd_extreme_comparisons)]
        if confirmations >= NB_CONFIRMATIONS {
            let closed_contract = ClosedContract {
                attestations: contract.attestations.clone(),
                signed_cet: Some(contract.signed_cet.clone()),
                contract_id: contract.signed_contract.accepted_contract.get_contract_id(),
                temporary_contract_id: contract
                    .signed_contract
                    .accepted_contract
                    .offered_contract
                    .id,
                counter_party_id: contract
                    .signed_contract
                    .accepted_contract
                    .offered_contract
                    .counter_party,
                pnl: contract
                    .signed_contract
                    .accepted_contract
                    .compute_pnl(&contract.signed_cet),
            };
            self.store
                .update_contract(&Contract::Closed(closed_contract))?;
        }

        Ok(())
    }

    fn close_contract(
        &self,
        contract: &SignedContract,
        signed_cet: Transaction,
        attestations: Vec<OracleAttestation>,
    ) -> Result<Contract, Error> {
        let confirmations = self
            .blockchain
            .get_transaction_confirmations(&signed_cet.txid())?;

        #[allow(clippy::absurd_extreme_comparisons)]
        if confirmations < 1 {
            // TODO(tibo): if this fails because another tx is already in
            // mempool or blockchain, we might have been cheated. There is
            // not much to be done apart from possibly extracting a fraud
            // proof but ideally it should be handled.
            self.blockchain.send_transaction(&signed_cet)?;

            let preclosed_contract = PreClosedContract {
                signed_contract: contract.clone(),
                attestations: Some(attestations),
                signed_cet,
            };

            return Ok(Contract::PreClosed(preclosed_contract));
        } else if confirmations < NB_CONFIRMATIONS {
            let preclosed_contract = PreClosedContract {
                signed_contract: contract.clone(),
                attestations: Some(attestations),
                signed_cet,
            };

            return Ok(Contract::PreClosed(preclosed_contract));
        }

        let closed_contract = ClosedContract {
            attestations: Some(attestations.to_vec()),
            pnl: contract.accepted_contract.compute_pnl(&signed_cet),
            signed_cet: Some(signed_cet),
            contract_id: contract.accepted_contract.get_contract_id(),
            temporary_contract_id: contract.accepted_contract.offered_contract.id,
            counter_party_id: contract.accepted_contract.offered_contract.counter_party,
        };

        Ok(Contract::Closed(closed_contract))
    }

    fn check_refund(&self, contract: &SignedContract) -> Result<(), Error> {
        // TODO(tibo): should check for confirmation of refund before updating state
        if contract
            .accepted_contract
            .dlc_transactions
            .refund
            .lock_time
            .0 as u64
            <= self.time.unix_time_now()
        {
            let accepted_contract = &contract.accepted_contract;
            let refund = accepted_contract.dlc_transactions.refund.clone();
            let confirmations = self
                .blockchain
                .get_transaction_confirmations(&refund.txid())?;
            if confirmations == 0 {
                let refund =
                    crate::contract_updater::get_signed_refund(&self.secp, contract, &self.wallet)?;
                self.blockchain.send_transaction(&refund)?;
            }

            self.store
                .update_contract(&Contract::Refunded(contract.clone()))?;
        }

        Ok(())
    }
}

impl<W: Deref, B: Deref, S: Deref, O: Deref, T: Deref, F: Deref> Manager<W, B, S, O, T, F>
where
    W::Target: Wallet,
    B::Target: Blockchain,
    S::Target: Storage,
    O::Target: Oracle,
    T::Target: Time,
    F::Target: FeeEstimator,
{
    /// Create a new channel offer and return the [`dlc_messages::channel::OfferChannel`]
    /// message to be sent to the `counter_party`.
    pub fn offer_channel(
        &self,
        contract_input: &ContractInput,
        counter_party: PublicKey,
        fee_config: dlc::FeeConfig,
        reference_id: Option<ReferenceId>,
    ) -> Result<OfferChannel, Error> {
        let oracle_announcements = contract_input
            .contract_infos
            .iter()
            .map(|x| self.get_oracle_announcements(&x.oracles))
            .collect::<Result<Vec<_>, Error>>()?;

        let (offered_channel, offered_contract) = crate::channel_updater::offer_channel(
            &self.secp,
            contract_input,
            &counter_party,
            &oracle_announcements,
            CET_NSEQUENCE,
            REFUND_DELAY,
            &self.wallet,
            &self.blockchain,
            &self.time,
            crate::utils::get_new_temporary_id(),
            fee_config,
            false,
            reference_id
        )?;

        let msg = offered_channel.get_offer_channel_msg(&offered_contract, reference_id);

        self.store.upsert_channel(
            Channel::Offered(offered_channel),
            Some(Contract::Offered(offered_contract)),
        )?;

        Ok(msg)
    }

    /// Reject a channel that was offered. Returns the [`dlc_messages::channel::Reject`]
    /// message to be sent as well as the public key of the offering node.
    pub fn reject_channel(&self, channel_id: &DlcChannelId) -> Result<(Reject, PublicKey), Error> {
        let offered_channel = get_channel_in_state!(self, channel_id, Offered, None as Option<PublicKey>)?;

        if offered_channel.is_offer_party {
            return Err(Error::InvalidState(
                "Cannot reject channel initiated by us.".to_string(),
            ));
        }

        let offered_contract = get_contract_in_state!(self, &offered_channel.offered_contract_id, Offered, None as Option<PublicKey>)?;

        let reference_id = offered_channel.reference_id;
        let counterparty = offered_channel.counter_party;
        self.store.upsert_channel(Channel::Cancelled(offered_channel), Some(Contract::Rejected(offered_contract)))?;

        let msg = Reject{ channel_id: *channel_id, timestamp: get_unix_time_now(), reference_id };
        Ok((msg, counterparty))
    }

    /// Accept a channel that was offered. Returns the [`dlc_messages::channel::AcceptChannel`]
    /// message to be sent, the updated [`DlcChannelId`] and [`crate::ContractId`],
    /// as well as the public key of the offering node.
    pub fn accept_channel(
        &self,
        channel_id: &DlcChannelId,
        fee_config: dlc::FeeConfig,
    ) -> Result<(AcceptChannel, DlcChannelId, ContractId, PublicKey), Error> {
        let offered_channel =
            get_channel_in_state!(self, channel_id, Offered, None as Option<PublicKey>)?;

        if offered_channel.is_offer_party {
            return Err(Error::InvalidState(
                "Cannot accept channel initiated by us.".to_string(),
            ));
        }

        let offered_contract = get_contract_in_state!(
            self,
            &offered_channel.offered_contract_id,
            Offered,
            None as Option<PublicKey>
        )?;

        let (accepted_channel, accepted_contract, accept_channel) =
            crate::channel_updater::accept_channel_offer(
                &self.secp,
                &offered_channel,
                &offered_contract,
                &self.wallet,
                &self.blockchain,
                fee_config,
            )?;

        self.wallet.import_address(&Address::p2wsh(
            &accepted_contract.dlc_transactions.funding_script_pubkey,
            self.blockchain.get_network()?,
        ))?;

        let channel_id = accepted_channel.channel_id;
        let contract_id = accepted_contract.get_contract_id();
        let counter_party = accepted_contract.offered_contract.counter_party;

        self.store.upsert_channel(
            Channel::Accepted(accepted_channel),
            Some(Contract::Accepted(accepted_contract)),
        )?;

        Ok((accept_channel, channel_id, contract_id, counter_party))
    }

    /// Force close the channel with given [`DlcChannelId`].
    pub fn force_close_channel(&self, channel_id: &DlcChannelId, reference_id: Option<ReferenceId>) -> Result<(), Error> {
        let channel = get_channel_in_state!(self, channel_id, Signed, None as Option<PublicKey>)?;

        self.force_close_channel_internal(channel, None, true, reference_id)
    }

    /// Offer to settle the balance of a channel so that the counter party gets
    /// `counter_payout`. Returns the [`dlc_messages::channel::SettleChannelOffer`]
    /// message to be sent and the public key of the counter party node.
    pub fn settle_offer(
        &self,
        channel_id: &DlcChannelId,
        counter_payout: u64,
        reference_id: Option<ReferenceId>
    ) -> Result<(SettleOffer, PublicKey), Error> {
        let mut signed_channel =
            get_channel_in_state!(self, channel_id, Signed, None as Option<PublicKey>)?;

        let msg = crate::channel_updater::settle_channel_offer(
            &self.secp,
            &mut signed_channel,
            counter_payout,
            PEER_TIMEOUT,
            &self.wallet,
            &self.time,
            reference_id,
        )?;

        let counter_party = signed_channel.counter_party;

        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)?;

        Ok((msg, counter_party))
    }

    /// Accept a settlement offer, returning the [`SettleAccept`] message to be
    /// sent to the node with the returned [`PublicKey`] id.
    pub fn accept_settle_offer(
        &self,
        channel_id: &DlcChannelId,
    ) -> Result<(SettleAccept, PublicKey), Error> {
        let mut signed_channel =
            get_channel_in_state!(self, channel_id, Signed, None as Option<PublicKey>)?;

        let own_settle_adaptor_sk = if let Some(sub_channel_id) =
            signed_channel.sub_channel_id.as_ref()
        {
            let (signed_sub_channel, state) =
                get_sub_channel_in_state!(self, *sub_channel_id, Signed, None::<PublicKey>)?;
            let own_base_secret_key = self
                .wallet
                .get_secret_key_for_pubkey(&signed_sub_channel.own_base_points.own_basepoint)?;
            let own_secret_key =
                derive_private_key(&self.secp, &state.own_per_split_point, &own_base_secret_key);
            Some(own_secret_key)
        } else {
            None
        };

        let msg = crate::channel_updater::settle_channel_accept_internal(
            &self.secp,
            &mut signed_channel,
            CET_NSEQUENCE,
            0,
            PEER_TIMEOUT,
            &self.wallet,
            &self.time,
            own_settle_adaptor_sk,
            &self.chain_monitor,
        )?;

        let counter_party = signed_channel.counter_party;

        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)?;

        Ok((msg, counter_party))
    }

    /// Returns a [`RenewOffer`] message as well as the [`PublicKey`] of the
    /// counter party's node to offer the establishment of a new contract in the
    /// channel.
    pub fn renew_offer(
        &self,
        channel_id: &DlcChannelId,
        counter_payout: u64,
        contract_input: &ContractInput,
        reference_id: Option<ReferenceId>
    ) -> Result<(RenewOffer, PublicKey), Error> {
        let mut signed_channel =
            get_channel_in_state!(self, channel_id, Signed, None as Option<PublicKey>)?;

        let oracle_announcements = contract_input
            .contract_infos
            .iter()
            .map(|x| self.get_oracle_announcements(&x.oracles))
            .collect::<Result<Vec<_>, Error>>()?;

        let (msg, offered_contract) = crate::channel_updater::renew_offer(
            &self.secp,
            &mut signed_channel,
            contract_input,
            oracle_announcements,
            counter_payout,
            REFUND_DELAY,
            PEER_TIMEOUT,
            CET_NSEQUENCE,
            &self.wallet,
            &self.time,
            reference_id,
        )?;

        let counter_party = offered_contract.counter_party;

        self.store.upsert_channel(
            Channel::Signed(signed_channel),
            Some(Contract::Offered(offered_contract)),
        )?;

        Ok((msg, counter_party))
    }

    /// Accept an offer to renew the contract in the channel. Returns the
    /// [`RenewAccept`] message to be sent to the peer with the returned
    /// [`PublicKey`] as node id.
    pub fn accept_renew_offer(
        &self,
        channel_id: &DlcChannelId,
    ) -> Result<(RenewAccept, PublicKey), Error> {
        let mut signed_channel =
            get_channel_in_state!(self, channel_id, Signed, None as Option<PublicKey>)?;
        let offered_contract_id = signed_channel.get_contract_id().ok_or_else(|| {
            Error::InvalidState("Expected to have a contract id but did not.".to_string())
        })?;

        let offered_contract = get_contract_in_state!(
            self,
            &offered_contract_id,
            Offered,
            None as Option<PublicKey>
        )?;

        let (accepted_contract, msg) = crate::channel_updater::accept_channel_renewal_internal(
            &self.secp,
            &mut signed_channel,
            &offered_contract,
            CET_NSEQUENCE,
            PEER_TIMEOUT,
            &self.wallet,
            &self.time,
        )?;

        let counter_party = signed_channel.counter_party;

        self.store.upsert_channel(
            Channel::Signed(signed_channel),
            Some(Contract::Accepted(accepted_contract)),
        )?;

        Ok((msg, counter_party))
    }

    /// Reject an offer to renew the contract in the channel. Returns the
    /// [`Reject`] message to be sent to the peer with the returned
    /// [`PublicKey`] node id.
    pub fn reject_renew_offer(
        &self,
        channel_id: &DlcChannelId,
    ) -> Result<(Reject, PublicKey), Error> {
        let mut signed_channel =
            get_channel_in_state!(self, channel_id, Signed, None as Option<PublicKey>)?;
        let offered_contract_id = signed_channel.get_contract_id().ok_or_else(|| {
            Error::InvalidState(
                "Expected to be in a state with an associated contract id but was not.".to_string(),
            )
        })?;

        let offered_contract = get_contract_in_state!(
            self,
            &offered_contract_id,
            Offered,
            None as Option<PublicKey>
        )?;

        let reject_msg = crate::channel_updater::reject_renew_offer(&mut signed_channel)?;

        let counter_party = signed_channel.counter_party;

        self.store.upsert_channel(
            Channel::Signed(signed_channel),
            Some(Contract::Rejected(offered_contract)),
        )?;

        Ok((reject_msg, counter_party))
    }

    /// Returns a [`Reject`] message to be sent to the counter party of the
    /// channel to inform them that the local party does not wish to accept the
    /// proposed settle offer.
    pub fn reject_settle_offer(
        &self,
        channel_id: &DlcChannelId,
    ) -> Result<(Reject, PublicKey), Error> {
        let mut signed_channel =
            get_channel_in_state!(self, channel_id, Signed, None as Option<PublicKey>)?;

        let msg = crate::channel_updater::reject_settle_offer(&mut signed_channel)?;

        let counter_party = signed_channel.counter_party;

        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)?;

        Ok((msg, counter_party))
    }

    /// Returns a [`CollaborativeCloseOffer`] message to be sent to the counter
    /// party of the channel and update the state of the channel. Note that the
    /// channel will be forced closed after a timeout if the counter party does
    /// not broadcast the close transaction.
    pub fn offer_collaborative_close(
        &self,
        channel_id: &DlcChannelId,
        counter_payout: u64,
        reference_id: Option<ReferenceId>
    ) -> Result<CollaborativeCloseOffer, Error> {
        let mut signed_channel =
            get_channel_in_state!(self, channel_id, Signed, None as Option<PublicKey>)?;

        let (msg, close_tx) = crate::channel_updater::offer_collaborative_close(
            &self.secp,
            &mut signed_channel,
            counter_payout,
            &self.wallet,
            &self.time,
            reference_id
        )?;

        self.chain_monitor.lock().unwrap().add_tx(
            close_tx.txid(),
            ChannelInfo {
                channel_id: *channel_id,
                tx_type: TxType::CollaborativeClose,
            },
        );

        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)?;
        self.store
            .persist_chain_monitor(&self.chain_monitor.lock().unwrap())?;

        Ok(msg)
    }

    /// Accept an offer to collaboratively close the channel. The close transaction
    /// will be broadcast and the state of the channel updated.
    pub fn accept_collaborative_close(&self, channel_id: &DlcChannelId) -> Result<(), Error> {
        let signed_channel =
            get_channel_in_state!(self, channel_id, Signed, None as Option<PublicKey>)?;

        let closed_contract = if let Some(SignedChannelState::Established {
            signed_contract_id,
            ..
        }) = &signed_channel.roll_back_state
        {
            let counter_payout = get_signed_channel_state!(
                signed_channel,
                CollaborativeCloseOffered,
                counter_payout
            )?;
            Some(self.get_collaboratively_closed_contract(
                signed_contract_id,
                *counter_payout,
                true,
            )?)
        } else {
            None
        };

        let (close_tx, closed_channel) = crate::channel_updater::accept_collaborative_close_offer(
            &self.secp,
            &signed_channel,
            &self.wallet,
        )?;

        self.blockchain.send_transaction(&close_tx)?;

        self.store.upsert_channel(closed_channel, None)?;

        if let Some(closed_contract) = closed_contract {
            self.store
                .update_contract(&Contract::Closed(closed_contract))?;
        }

        Ok(())
    }

    fn try_finalize_closing_established_channel(
        &self,
        signed_channel: SignedChannel,
    ) -> Result<(), Error> {
        let (buffer_tx, contract_id, &is_initiator) = get_signed_channel_state!(
            signed_channel,
            Closing,
            buffer_transaction,
            contract_id,
            is_initiator
        )?;

        if self
            .blockchain
            .get_transaction_confirmations(&buffer_tx.txid())?
            >= CET_NSEQUENCE
        {
            log::info!(
                "Buffer transaction for contract {} has enough confirmations to spend from it",
                serialize_hex(&contract_id)
            );

            let confirmed_contract =
                get_contract_in_state!(self, contract_id, Confirmed, None as Option<PublicKey>)?;

            let (contract_info, adaptor_info, attestations) = self
                .get_closable_contract_info(&confirmed_contract)
                .ok_or_else(|| {
                    Error::InvalidState("Could not get information to close contract".to_string())
                })?;

            let (signed_cet, closed_channel) =
                channel_updater::finalize_unilateral_close_established_channel(
                    &self.secp,
                    &signed_channel,
                    &confirmed_contract,
                    contract_info,
                    &attestations,
                    adaptor_info,
                    &self.wallet,
                    is_initiator,
                )?;

            let closed_contract = self.close_contract(
                &confirmed_contract,
                signed_cet,
                attestations.iter().map(|x| &x.1).cloned().collect(),
            )?;

            self.chain_monitor
                .lock()
                .unwrap()
                .cleanup_channel(signed_channel.channel_id);

            self.store
                .upsert_channel(closed_channel, Some(closed_contract))?;
        }

        Ok(())
    }

    fn try_finalize_settled_closing_channel(
        &self,
        signed_channel: SignedChannel,
    ) -> Result<(), Error> {
        let (settle_tx, &is_initiator) = get_signed_channel_state!(
            signed_channel,
            SettledClosing,
            settle_transaction,
            is_initiator
        )?;

        if self
            .blockchain
            .get_transaction_confirmations(&settle_tx.txid())?
            >= CET_NSEQUENCE
        {
            log::info!(
                "Settle transaction {} for channel {} has enough confirmations to spend from it",
                settle_tx.txid(),
                serialize_hex(&signed_channel.channel_id),
            );

            let fee_rate_per_vb: u64 = {
                let fee_rate = self
                    .fee_estimator
                    .get_est_sat_per_1000_weight(
                        ConfirmationTarget::Background,
                    );

                let fee_rate = fee_rate / 250;

                fee_rate.into()
            };

            let (claim_tx, settled_closing_channel) =
                channel_updater::finalize_unilateral_close_settled_channel(
                    &self.secp,
                    &signed_channel,
                    &self.wallet.get_new_address()?,
                    fee_rate_per_vb,
                    &self.wallet,
                    is_initiator,
                )?;

            let confirmations = self
                .blockchain
                .get_transaction_confirmations(&claim_tx.txid())?;

            if confirmations < 1 {
                self.blockchain.send_transaction(&claim_tx)?;
            }

            self.store
                .upsert_channel(settled_closing_channel, None)?;
        }

        Ok(())
    }

    fn try_confirm_claim_tx(&self, channel: &SettledClosingChannel) -> Result<(), Error> {
        let claim_tx = &channel.claim_transaction;

        let confirmations = self
            .blockchain
            .get_transaction_confirmations(&claim_tx.txid())?;

        // TODO(lucas): No need to send it again if it is in mempool, unless we want to bump the
        // fee.
        #[allow(clippy::absurd_extreme_comparisons)]
        if confirmations < 1 {
            self.blockchain.send_transaction(claim_tx)?;
        } else if confirmations >= NB_CONFIRMATIONS {
            self.chain_monitor
                .lock()
                .unwrap()
                .cleanup_channel(channel.channel_id);

            let closed_channel = if channel.is_closer {
                Channel::Closed(ClosedChannel {
                    counter_party: channel.counter_party,
                    temporary_channel_id: channel.temporary_channel_id,
                    channel_id: channel.channel_id,
                    reference_id: channel.reference_id,
                    // TODO(lucas): We are losing the context of the settle transaction here, but it
                    // can always be found based on the claim transaction. We could introduce a
                    // dedicated `SettledClosed` state to fix this.
                    closing_txid: channel.claim_transaction.txid()
                })
            } else {
                Channel::CounterClosed(ClosedChannel {
                    counter_party: channel.counter_party,
                    temporary_channel_id: channel.temporary_channel_id,
                    channel_id: channel.channel_id,
                    reference_id: channel.reference_id,
                    closing_txid: channel.claim_transaction.txid()
                })
            };

            self.chain_monitor
                .lock()
                .unwrap()
                .cleanup_channel(channel.channel_id);

            self.store
                .upsert_channel(closed_channel, None)?;
        }

        Ok(())
    }

    fn on_offer_channel(
        &self,
        offer_channel: &OfferChannel,
        counter_party: PublicKey,
    ) -> Result<(), Error> {
        offer_channel.validate(
            &self.secp,
            REFUND_DELAY,
            REFUND_DELAY * 2,
            CET_NSEQUENCE,
            CET_NSEQUENCE * 2,
        )?;

        let (channel, contract) = OfferedChannel::from_offer_channel(offer_channel, counter_party)?;

        contract.validate()?;

        if self
            .store
            .get_channel(&channel.temporary_channel_id)?
            .is_some()
        {
            return Err(Error::InvalidParameters(
                "Channel with identical idea already in store".to_string(),
            ));
        }

        self.store
            .upsert_channel(Channel::Offered(channel), Some(Contract::Offered(contract)))?;

        Ok(())
    }

    fn on_accept_channel(
        &self,
        accept_channel: &AcceptChannel,
        peer_id: &PublicKey,
    ) -> Result<SignChannel, Error> {
        let offered_channel = get_channel_in_state!(
            self,
            &accept_channel.temporary_channel_id,
            Offered,
            Some(*peer_id)
        )?;
        let offered_contract = get_contract_in_state!(
            self,
            &offered_channel.offered_contract_id,
            Offered,
            Some(*peer_id)
        )?;

        let (signed_channel, signed_contract, sign_channel) = {
            let res = crate::channel_updater::verify_and_sign_accepted_channel(
                &self.secp,
                &offered_channel,
                &offered_contract,
                accept_channel,
                //TODO(tibo): this should be parameterizable.
                CET_NSEQUENCE,
                &self.wallet,
                &self.chain_monitor,
                offered_channel
                    .fee_config
                    .map(FeeConfig::from)
                    .unwrap_or(FeeConfig::EvenSplit),
            );

            match res {
                Ok(res) => res,
                Err(e) => {
                    let channel = crate::channel::FailedAccept {
                        temporary_channel_id: accept_channel.temporary_channel_id,
                        error_message: format!("Error validating accept channel: {e}"),
                        accept_message: accept_channel.clone(),
                        counter_party: *peer_id,
                        reference_id: accept_channel.reference_id
                    };
                    self.store
                        .upsert_channel(Channel::FailedAccept(channel), None)?;
                    return Err(e);
                }
            }
        };

        self.wallet.import_address(&Address::p2wsh(
            &signed_contract
                .accepted_contract
                .dlc_transactions
                .funding_script_pubkey,
            self.blockchain.get_network()?,
        ))?;

        log::info!(
            "Signed fund transaction {} for channel {} and contract {}",
            signed_contract
                .accepted_contract
                .dlc_transactions
                .fund
                .txid(),
            signed_channel.channel_id.to_hex(),
            signed_contract.accepted_contract.get_contract_id().to_hex()
        );

        self.store.upsert_channel(
            Channel::Signed(signed_channel),
            Some(Contract::Signed(signed_contract)),
        )?;

        self.store
            .persist_chain_monitor(&self.chain_monitor.lock().unwrap())?;

        Ok(sign_channel)
    }

    fn on_sign_channel(
        &self,
        sign_channel: &SignChannel,
        peer_id: &PublicKey,
    ) -> Result<(), Error> {
        let accepted_channel =
            get_channel_in_state!(self, &sign_channel.channel_id, Accepted, Some(*peer_id))?;
        let accepted_contract = get_contract_in_state!(
            self,
            &accepted_channel.accepted_contract_id,
            Accepted,
            Some(*peer_id)
        )?;

        let (signed_channel, signed_contract, signed_fund_tx) = {
            let res = verify_signed_channel(
                &self.secp,
                &accepted_channel,
                &accepted_contract,
                sign_channel,
                &self.wallet,
                &self.chain_monitor,
            );

            match res {
                Ok(res) => res,
                Err(e) => {
                    let channel = crate::channel::FailedSign {
                        channel_id: sign_channel.channel_id,
                        error_message: format!("Error validating accept channel: {e}"),
                        sign_message: sign_channel.clone(),
                        counter_party: *peer_id,
                        reference_id: accepted_channel.reference_id
                    };
                    self.store
                        .upsert_channel(Channel::FailedSign(channel), None)?;
                    return Err(e);
                }
            }
        };

        // TODO(lucas): Maybe we just delete this.
        if let SignedChannelState::Established {
            buffer_transaction, ..
        } = &signed_channel.state
        {
            self.chain_monitor.lock().unwrap().add_tx(
                buffer_transaction.txid(),
                ChannelInfo {
                    channel_id: signed_channel.channel_id,
                    tx_type: TxType::BufferTx,
                },
            );
        } else {
            unreachable!();
        }

        log::info!(
            "Broadcasting fund transaction {} for channel {} and contract {}",
            signed_fund_tx.txid(),
            sign_channel.channel_id.to_hex(),
            signed_contract.accepted_contract.get_contract_id().to_hex()
        );

        self.blockchain.send_transaction(&signed_fund_tx)?;

        self.store.upsert_channel(
            Channel::Signed(signed_channel),
            Some(Contract::Signed(signed_contract)),
        )?;
        self.store
            .persist_chain_monitor(&self.chain_monitor.lock().unwrap())?;

        Ok(())
    }

    fn on_settle_offer(
        &self,
        settle_offer: &SettleOffer,
        peer_id: &PublicKey,
    ) -> Result<Option<Reject>, Error> {
        let mut signed_channel =
            get_channel_in_state!(self, &settle_offer.channel_id, Signed, Some(*peer_id))?;

        if let SignedChannelState::SettledOffered { .. } = signed_channel.state {
            return Ok(Some(Reject {
                channel_id: settle_offer.channel_id,
                timestamp: get_unix_time_now(),
                reference_id: settle_offer.reference_id
            }));
        }

        crate::channel_updater::on_settle_offer(&mut signed_channel, settle_offer)?;

        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)?;

        Ok(None)
    }

    fn on_settle_accept(
        &self,
        settle_accept: &SettleAccept,
        peer_id: &PublicKey,
    ) -> Result<SettleConfirm, Error> {
        let mut signed_channel =
            get_channel_in_state!(self, &settle_accept.channel_id, Signed, Some(*peer_id))?;

        let (own_settle_adaptor_sk, counter_settle_adaptor_pk) = if let Some(sub_channel_id) =
            signed_channel.sub_channel_id
        {
            let (signed_sub_channel, state) =
                get_sub_channel_in_state!(self, sub_channel_id, Signed, None::<PublicKey>)?;
            let own_base_secret_key = self
                .wallet
                .get_secret_key_for_pubkey(&signed_sub_channel.own_base_points.own_basepoint)?;
            let own_secret_key =
                derive_private_key(&self.secp, &state.own_per_split_point, &own_base_secret_key);
            let accept_revoke_params = signed_sub_channel
                .counter_base_points
                .expect("to have counter base points")
                .get_revokable_params(
                    &self.secp,
                    &signed_sub_channel.own_base_points.revocation_basepoint,
                    &state.counter_per_split_point,
                );
            (
                Some(own_secret_key),
                Some(accept_revoke_params.own_pk.inner),
            )
        } else {
            (None, None)
        };

        let msg = crate::channel_updater::settle_channel_confirm_internal(
            &self.secp,
            &mut signed_channel,
            settle_accept,
            CET_NSEQUENCE,
            0,
            PEER_TIMEOUT,
            &self.wallet,
            &self.time,
            own_settle_adaptor_sk,
            counter_settle_adaptor_pk,
            &self.chain_monitor,
        )?;

        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)?;

        Ok(msg)
    }

    fn on_settle_confirm(
        &self,
        settle_confirm: &SettleConfirm,
        peer_id: &PublicKey,
    ) -> Result<SettleFinalize, Error> {
        let mut signed_channel =
            get_channel_in_state!(self, &settle_confirm.channel_id, Signed, Some(*peer_id))?;
        let &own_payout = get_signed_channel_state!(signed_channel, SettledAccepted, own_payout)?;
        let (prev_buffer_tx, own_buffer_adaptor_signature, is_offer, signed_contract_id) = get_signed_channel_rollback_state!(
            signed_channel,
            Established,
            buffer_transaction,
            own_buffer_adaptor_signature,
            is_offer,
            signed_contract_id
        )?;

        let prev_buffer_txid = prev_buffer_tx.txid();
        let own_buffer_adaptor_signature = *own_buffer_adaptor_signature;
        let is_offer = *is_offer;
        let signed_contract_id = *signed_contract_id;

        let counter_settle_adaptor_pk =
            if let Some(sub_channel_id) = signed_channel.sub_channel_id.as_ref() {
                let (signed_sub_channel, state) =
                    get_sub_channel_in_state!(self, *sub_channel_id, Signed, None::<PublicKey>)?;
                let accept_revoke_params = signed_sub_channel
                    .counter_base_points
                    .expect("to have counter base points")
                    .get_revokable_params(
                        &self.secp,
                        &signed_sub_channel.own_base_points.revocation_basepoint,
                        &state.counter_per_split_point,
                    );
                Some(accept_revoke_params.own_pk.inner)
            } else {
                None
            };

        let msg = crate::channel_updater::settle_channel_finalize_internal(
            &self.secp,
            &mut signed_channel,
            settle_confirm,
            &self.wallet,
            counter_settle_adaptor_pk,
        )?;

        self.chain_monitor.lock().unwrap().add_tx(
            prev_buffer_txid,
            ChannelInfo {
                channel_id: signed_channel.channel_id,
                tx_type: TxType::Revoked {
                    update_idx: signed_channel.update_idx + 1,
                    own_adaptor_signature: own_buffer_adaptor_signature,
                    is_offer,
                    revoked_tx_type: RevokedTxType::Buffer,
                },
            },
        );

        let closed_contract = Contract::Closed(self.get_collaboratively_closed_contract(
            &signed_contract_id,
            own_payout,
            true,
        )?);

        self.store
            .upsert_channel(Channel::Signed(signed_channel), Some(closed_contract))?;
        self.store
            .persist_chain_monitor(&self.chain_monitor.lock().unwrap())?;

        Ok(msg)
    }

    fn on_settle_finalize(
        &self,
        settle_finalize: &SettleFinalize,
        peer_id: &PublicKey,
    ) -> Result<(), Error> {
        let mut signed_channel =
            get_channel_in_state!(self, &settle_finalize.channel_id, Signed, Some(*peer_id))?;
        let &own_payout = get_signed_channel_state!(signed_channel, SettledConfirmed, own_payout)?;
        let (buffer_tx, own_buffer_adaptor_signature, is_offer, signed_contract_id) = get_signed_channel_rollback_state!(
            signed_channel,
            Established,
            buffer_transaction,
            own_buffer_adaptor_signature,
            is_offer,
            signed_contract_id
        )?;

        let own_buffer_adaptor_signature = *own_buffer_adaptor_signature;
        let is_offer = *is_offer;
        let buffer_txid = buffer_tx.txid();
        let signed_contract_id = *signed_contract_id;

        crate::channel_updater::settle_channel_on_finalize(
            &self.secp,
            &mut signed_channel,
            settle_finalize,
        )?;

        self.chain_monitor.lock().unwrap().add_tx(
            buffer_txid,
            ChannelInfo {
                channel_id: signed_channel.channel_id,
                tx_type: TxType::Revoked {
                    update_idx: signed_channel.update_idx + 1,
                    own_adaptor_signature: own_buffer_adaptor_signature,
                    is_offer,
                    revoked_tx_type: RevokedTxType::Buffer,
                },
            },
        );

        let closed_contract = Contract::Closed(self.get_collaboratively_closed_contract(
            &signed_contract_id,
            own_payout,
            true,
        )?);
        self.store
            .upsert_channel(Channel::Signed(signed_channel), Some(closed_contract))?;
        self.store
            .persist_chain_monitor(&self.chain_monitor.lock().unwrap())?;

        Ok(())
    }

    fn on_renew_offer(
        &self,
        renew_offer: &RenewOffer,
        peer_id: &PublicKey,
    ) -> Result<Option<Reject>, Error> {
        let mut signed_channel =
            get_channel_in_state!(self, &renew_offer.channel_id, Signed, Some(*peer_id))?;

        // Received a renew offer when we already sent one, we reject it.
        if let SignedChannelState::RenewOffered { is_offer, .. } = signed_channel.state {
            if is_offer {
                return Ok(Some(Reject {
                    channel_id: renew_offer.channel_id,
                    timestamp: get_unix_time_now(),
                    reference_id: renew_offer.reference_id,
                }));
            }
        }

        let offered_contract =
            crate::channel_updater::on_renew_offer(&mut signed_channel, renew_offer,PEER_TIMEOUT, &self.time)?;

        self.store.create_contract(&offered_contract)?;
        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)?;

        Ok(None)
    }

    fn on_renew_accept(
        &self,
        renew_accept: &RenewAccept,
        peer_id: &PublicKey,
    ) -> Result<RenewConfirm, Error> {
        let mut signed_channel =
            get_channel_in_state!(self, &renew_accept.channel_id, Signed, Some(*peer_id))?;
        let offered_contract_id = signed_channel.get_contract_id().ok_or_else(|| {
            Error::InvalidState(
                "Expected to be in a state with an associated contract id but was not.".to_string(),
            )
        })?;

        let offered_contract =
            get_contract_in_state!(self, &offered_contract_id, Offered, Some(*peer_id))?;

        let own_buffer_adaptor_sk = if let Some(sub_channel_id) =
            signed_channel.sub_channel_id.as_ref()
        {
            let (signed_sub_channel, state) =
                get_sub_channel_in_state!(self, *sub_channel_id, Signed, None::<PublicKey>)?;
            let own_base_secret_key = self
                .wallet
                .get_secret_key_for_pubkey(&signed_sub_channel.own_base_points.own_basepoint)?;
            let own_secret_key =
                derive_private_key(&self.secp, &state.own_per_split_point, &own_base_secret_key);

            Some(own_secret_key)
        } else {
            None
        };

        let (signed_contract, msg) =
            crate::channel_updater::verify_renew_accept_and_confirm_internal(
                &self.secp,
                renew_accept,
                &mut signed_channel,
                &offered_contract,
                CET_NSEQUENCE,
                PEER_TIMEOUT,
                &self.wallet,
                &self.time,
                own_buffer_adaptor_sk,
            )?;

        // Directly confirmed as we're in a channel the fund tx is already confirmed.
        self.store.upsert_channel(
            Channel::Signed(signed_channel),
            Some(Contract::Confirmed(signed_contract)),
        )?;

        Ok(msg)
    }

    fn on_renew_confirm(
        &self,
        renew_confirm: &RenewConfirm,
        peer_id: &PublicKey,
    ) -> Result<RenewFinalize, Error> {
        let mut signed_channel =
            get_channel_in_state!(self, &renew_confirm.channel_id, Signed, Some(*peer_id))?;
        let own_payout = get_signed_channel_state!(signed_channel, RenewAccepted, own_payout)?;
        let contract_id = signed_channel.get_contract_id().ok_or_else(|| {
            Error::InvalidState(
                "Expected to be in a state with an associated contract id but was not.".to_string(),
            )
        })?;

        let (tx_type, prev_tx_id, closed_contract) = match signed_channel
            .roll_back_state
            .as_ref()
            .expect("to have a rollback state")
        {
            SignedChannelState::Established {
                own_buffer_adaptor_signature,
                buffer_transaction,
                signed_contract_id,
                ..
            } => {
                let closed_contract = Contract::Closed(self.get_collaboratively_closed_contract(
                    signed_contract_id,
                    *own_payout,
                    true,
                )?);
                (
                    TxType::Revoked {
                        update_idx: signed_channel.update_idx,
                        own_adaptor_signature: *own_buffer_adaptor_signature,
                        is_offer: false,
                        revoked_tx_type: RevokedTxType::Buffer,
                    },
                    buffer_transaction.txid(),
                    Some(closed_contract),
                )
            }
            SignedChannelState::Settled {
                settle_tx,
                own_settle_adaptor_signature,
                ..
            } => (
                TxType::Revoked {
                    update_idx: signed_channel.update_idx,
                    own_adaptor_signature: *own_settle_adaptor_signature,
                    is_offer: false,
                    revoked_tx_type: RevokedTxType::Settle,
                },
                settle_tx.txid(),
                None,
            ),
            s => {
                return Err(Error::InvalidState(format!(
                    "Expected rollback state Established or Revoked but found {s:?}"
                )))
            }
        };
        let accepted_contract =
            get_contract_in_state!(self, &contract_id, Accepted, Some(*peer_id))?;

        let (counter_buffer_adaptor_pk, own_buffer_adaptor_sk) = if let Some(sub_channel_id) =
            signed_channel.sub_channel_id.as_ref()
        {
            let (signed_sub_channel, state) =
                get_sub_channel_in_state!(self, *sub_channel_id, Signed, None::<PublicKey>)?;
            let accept_revoke_params = signed_sub_channel
                .counter_base_points
                .expect("to have counter base points")
                .get_revokable_params(
                    &self.secp,
                    &signed_sub_channel.own_base_points.revocation_basepoint,
                    &state.counter_per_split_point,
                );
            let (signed_sub_channel, state) =
                get_sub_channel_in_state!(self, *sub_channel_id, Signed, None::<PublicKey>)?;
            let own_base_secret_key = self
                .wallet
                .get_secret_key_for_pubkey(&signed_sub_channel.own_base_points.own_basepoint)?;
            let own_secret_key =
                derive_private_key(&self.secp, &state.own_per_split_point, &own_base_secret_key);

            (
                Some(accept_revoke_params.own_pk.inner),
                Some(own_secret_key),
            )
        } else {
            (None, None)
        };

        let (signed_contract, msg) =
            crate::channel_updater::verify_renew_confirm_and_finalize_internal(
                &self.secp,
                &mut signed_channel,
                &accepted_contract,
                renew_confirm,
                PEER_TIMEOUT,
                &self.time,
                &self.wallet,
                counter_buffer_adaptor_pk,
                own_buffer_adaptor_sk,
                &self.chain_monitor,
            )?;

        self.chain_monitor.lock().unwrap().add_tx(
            prev_tx_id,
            ChannelInfo {
                channel_id: signed_channel.channel_id,
                tx_type,
            },
        );

        // Directly confirmed as we're in a channel the fund tx is already confirmed.
        self.store.upsert_channel(
            Channel::Signed(signed_channel),
            Some(Contract::Confirmed(signed_contract)),
        )?;

        self.store
            .persist_chain_monitor(&self.chain_monitor.lock().unwrap())?;

        if let Some(closed_contract) = closed_contract {
            self.store.update_contract(&closed_contract)?;
        }

        Ok(msg)
    }

    fn on_renew_finalize(
        &self,
        renew_finalize: &RenewFinalize,
        peer_id: &PublicKey,
    ) -> Result<RenewRevoke, Error> {
        let mut signed_channel =
            get_channel_in_state!(self, &renew_finalize.channel_id, Signed, Some(*peer_id))?;
        let own_payout = get_signed_channel_state!(signed_channel, RenewConfirmed, own_payout)?;

        let (tx_type, prev_tx_id, closed_contract) = match signed_channel
            .roll_back_state
            .as_ref()
            .expect("to have a rollback state")
        {
            SignedChannelState::Established {
                own_buffer_adaptor_signature,
                buffer_transaction,
                signed_contract_id,
                ..
            } => {
                let closed_contract = self.get_collaboratively_closed_contract(
                    signed_contract_id,
                    *own_payout,
                    true,
                )?;
                (
                    TxType::Revoked {
                        update_idx: signed_channel.update_idx,
                        own_adaptor_signature: *own_buffer_adaptor_signature,
                        is_offer: false,
                        revoked_tx_type: RevokedTxType::Buffer,
                    },
                    buffer_transaction.txid(),
                    Some(Contract::Closed(closed_contract)),
                )
            }
            SignedChannelState::Settled {
                settle_tx,
                own_settle_adaptor_signature,
                ..
            } => (
                TxType::Revoked {
                    update_idx: signed_channel.update_idx,
                    own_adaptor_signature: *own_settle_adaptor_signature,
                    is_offer: false,
                    revoked_tx_type: RevokedTxType::Settle,
                },
                settle_tx.txid(),
                None,
            ),
            s => {
                return Err(Error::InvalidState(format!(
                    "Expected rollback state of Established or Settled but was {s:?}"
                )))
            }
        };

        let counter_buffer_adaptor_pk =
            if let Some(sub_channel_id) = signed_channel.sub_channel_id.as_ref() {
                let (signed_sub_channel, state) =
                    get_sub_channel_in_state!(self, *sub_channel_id, Signed, None::<PublicKey>)?;
                let accept_revoke_params = signed_sub_channel
                    .counter_base_points
                    .expect("to have counter base points")
                    .get_revokable_params(
                        &self.secp,
                        &signed_sub_channel.own_base_points.revocation_basepoint,
                        &state.counter_per_split_point,
                    );
                Some(accept_revoke_params.own_pk.inner)
            } else {
                None
            };

        let msg = crate::channel_updater::renew_channel_on_finalize(
            &self.secp,
            &mut signed_channel,
            renew_finalize,
            counter_buffer_adaptor_pk,
            &self.wallet,
        )?;

        self.chain_monitor.lock().unwrap().add_tx(
            prev_tx_id,
            ChannelInfo {
                channel_id: signed_channel.channel_id,
                tx_type,
            },
        );

        let buffer_tx =
            get_signed_channel_state!(signed_channel, Established, ref buffer_transaction)?;

        self.chain_monitor.lock().unwrap().add_tx(
            buffer_tx.txid(),
            ChannelInfo {
                channel_id: signed_channel.channel_id,
                tx_type: TxType::BufferTx,
            },
        );

        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)?;
        self.store
            .persist_chain_monitor(&self.chain_monitor.lock().unwrap())?;

        if let Some(closed_contract) = closed_contract {
            self.store.update_contract(&closed_contract)?;
        }

        Ok(msg)
    }

    fn on_renew_revoke(
        &self,
        renew_revoke: &RenewRevoke,
        peer_id: &PublicKey,
    ) -> Result<(), Error> {
        let mut signed_channel =
            get_channel_in_state!(self, &renew_revoke.channel_id, Signed, Some(*peer_id))?;

        crate::channel_updater::renew_channel_on_revoke(
            &self.secp,
            &mut signed_channel,
            renew_revoke,
        )?;

        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)
    }

    fn on_collaborative_close_offer(
        &self,
        close_offer: &CollaborativeCloseOffer,
        peer_id: &PublicKey,
    ) -> Result<(), Error> {
        let mut signed_channel =
            get_channel_in_state!(self, &close_offer.channel_id, Signed, Some(*peer_id))?;

        crate::channel_updater::on_collaborative_close_offer(
            &mut signed_channel,
            close_offer,
            PEER_TIMEOUT,
            &self.time,
        )?;

        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)?;

        Ok(())
    }

    fn on_reject(&self, reject: &Reject, counter_party: &PublicKey) -> Result<(), Error> {
        let channel = self.store.get_channel(&reject.channel_id)?;

        if let Some(channel) = channel {
            if channel.get_counter_party_id() != *counter_party {
                return Err(Error::InvalidParameters(format!(
                    "Peer {:02x?} is not involved with {} {:02x?}.",
                    counter_party,
                    stringify!(Channel),
                    channel.get_id()
                )));
            }
            match channel {
                Channel::Offered(offered_channel) => {
                    let offered_contract = get_contract_in_state!(self, &offered_channel.offered_contract_id, Offered, None as Option<PublicKey>)?;

                    let utxos = offered_contract.funding_inputs_info.iter().map(|funding_input_info| {
                        let txid = Transaction::consensus_decode(&mut funding_input_info.funding_input.prev_tx.as_slice())
                            .expect("Transaction Decode Error")
                            .txid();
                        let vout = funding_input_info.funding_input.prev_tx_vout;
                        OutPoint{txid, vout}
                    }).collect::<Vec<_>>();

                    self.wallet.unreserve_utxos(&utxos)?;

                    // remove rejected channel, since nothing has been confirmed on chain yet.
                    self.store.upsert_channel(Channel::Cancelled(offered_channel), Some(Contract::Rejected(offered_contract)))?;
                },
                Channel::Signed(mut signed_channel) => {
                    let contract = match signed_channel.state {
                        SignedChannelState::RenewOffered { offered_contract_id, .. } => {
                            let offered_contract = get_contract_in_state!(self, &offered_contract_id, Offered, None::<PublicKey>)?;
                            Some(Contract::Rejected(offered_contract))

                        }
                        _ => None
                    };

                    crate::channel_updater::on_reject(&mut signed_channel)?;

                    self.store
                        .upsert_channel(Channel::Signed(signed_channel), contract)?;
                },
                channel => {
                    return Err(Error::InvalidState(
                        format!("Not in a state adequate to receive a reject message. {:?}", channel),
                    ))
                }
            }
        } else {
            warn!("Couldn't find rejected dlc channel with id: {}", reject.channel_id.to_hex());
        }

        Ok(())
    }

    fn channel_checks(&self) -> Result<(), Error> {
        let established_closing_channels = self
            .store
            .get_signed_channels(Some(SignedChannelStateType::Closing))?;

        for channel in established_closing_channels {
            if let Err(e) = self.try_finalize_closing_established_channel(channel) {
                error!("Error trying to close established channel: {}", e);
            }
        }

        let signed_settled_closing_channels = self
            .store
            .get_signed_channels(Some(SignedChannelStateType::SettledClosing))?;

        for channel in signed_settled_closing_channels {
            if let Err(e) = self.try_finalize_settled_closing_channel(channel) {
                error!("Error trying to close settled channel: {}", e);
            }
        }

        let settled_closing_channels = self
            .store
            .get_settled_closing_channels()?;

        for channel in settled_closing_channels {
            if let Err(e) = self.try_confirm_claim_tx(&channel) {
                error!("Error trying to confirm claim TX of settled channel: {}", e);
            }
        }

        if let Err(e) = self.check_for_timed_out_channels() {
            error!("Error checking timed out channels {}", e);
        }
        self.check_for_watched_tx()
    }

    fn check_for_timed_out_channels(&self) -> Result<(), Error> {
        check_for_timed_out_channels!(self, RenewOffered);
        check_for_timed_out_channels!(self, RenewAccepted);
        check_for_timed_out_channels!(self, RenewConfirmed);
        check_for_timed_out_channels!(self, RenewFinalized);
        check_for_timed_out_channels!(self, SettledOffered);
        check_for_timed_out_channels!(self, SettledAccepted);
        check_for_timed_out_channels!(self, SettledConfirmed);

        Ok(())
    }

    pub(crate) fn process_watched_txs(
        &self,
        watched_txs: Vec<(Transaction, ChannelInfo)>,
    ) -> Result<(), Error> {
        for (tx, channel_info) in watched_txs {
            let mut signed_channel = match get_channel_in_state!(
                self,
                &channel_info.channel_id,
                Signed,
                None as Option<PublicKey>
            ) {
                Ok(c) => c,
                Err(e) => {
                    error!(
                        "Could not retrieve channel {:?}: {}",
                        channel_info.channel_id, e
                    );
                    continue;
                }
            };

            let persist = match channel_info.tx_type {
                TxType::BufferTx => {
                    // TODO(tibo): should only considered closed after some confirmations.
                    // Ideally should save previous state, and maybe restore in
                    // case of reorg, though if the counter party has sent the
                    // tx to close the channel it is unlikely that the tx will
                    // not be part of a future block.

                    let contract_id = signed_channel
                        .get_contract_id()
                        .expect("to have a contract id");
                    let mut state = SignedChannelState::Closing {
                        buffer_transaction: tx.clone(),
                        is_initiator: false,
                        contract_id,
                    };
                    std::mem::swap(&mut signed_channel.state, &mut state);

                    signed_channel.roll_back_state = Some(state);

                    self.store
                        .upsert_channel(Channel::Signed(signed_channel), None)?;

                    false
                }
                TxType::Revoked {
                    update_idx,
                    own_adaptor_signature,
                    is_offer,
                    revoked_tx_type,
                } => {
                    let secret = signed_channel
                        .counter_party_commitment_secrets
                        .get_secret(update_idx)
                        .expect("to be able to retrieve the per update secret");
                    let counter_per_update_secret = SecretKey::from_slice(&secret)
                        .expect("to be able to parse the counter per update secret.");

                    let per_update_seed_pk = signed_channel.own_per_update_seed;

                    let per_update_seed_sk =
                        self.wallet.get_secret_key_for_pubkey(&per_update_seed_pk)?;

                    let per_update_secret = SecretKey::from_slice(&build_commitment_secret(
                        per_update_seed_sk.as_ref(),
                        update_idx,
                    ))
                    .expect("a valid secret key.");

                    let per_update_point =
                        PublicKey::from_secret_key(&self.secp, &per_update_secret);

                    let own_revocation_params = signed_channel.own_points.get_revokable_params(
                        &self.secp,
                        &signed_channel.counter_points.revocation_basepoint,
                        &per_update_point,
                    );

                    let counter_per_update_point =
                        PublicKey::from_secret_key(&self.secp, &counter_per_update_secret);

                    let base_own_sk = self
                        .wallet
                        .get_secret_key_for_pubkey(&signed_channel.own_points.own_basepoint)?;

                    let own_sk = derive_private_key(&self.secp, &per_update_point, &base_own_sk);

                    let counter_revocation_params =
                        signed_channel.counter_points.get_revokable_params(
                            &self.secp,
                            &signed_channel.own_points.revocation_basepoint,
                            &counter_per_update_point,
                        );

                    let witness = if signed_channel.own_params.fund_pubkey
                        < signed_channel.counter_params.fund_pubkey
                    {
                        tx.input[0].witness.to_vec().remove(1)
                    } else {
                        tx.input[0].witness.to_vec().remove(2)
                    };

                    let sig_data = witness
                        .iter()
                        .take(witness.len() - 1)
                        .cloned()
                        .collect::<Vec<_>>();
                    let own_sig = Signature::from_der(&sig_data)?;

                    let counter_sk = own_adaptor_signature.recover(
                        &self.secp,
                        &own_sig,
                        &counter_revocation_params.publish_pk.inner,
                    )?;

                    let own_revocation_base_secret = &self.wallet.get_secret_key_for_pubkey(
                        &signed_channel.own_points.revocation_basepoint,
                    )?;

                    let counter_revocation_sk = derive_private_revocation_key(
                        &self.secp,
                        &counter_per_update_secret,
                        own_revocation_base_secret,
                    );

                    let (offer_params, accept_params) = if is_offer {
                        (&own_revocation_params, &counter_revocation_params)
                    } else {
                        (&counter_revocation_params, &own_revocation_params)
                    };

                    let fee_rate_per_vb: u64 = (self.fee_estimator.get_est_sat_per_1000_weight(
                        lightning::chain::chaininterface::ConfirmationTarget::HighPriority,
                    ) / 250)
                        .into();

                    let signed_tx = match revoked_tx_type {
                        RevokedTxType::Buffer => {
                            dlc::channel::create_and_sign_punish_buffer_transaction(
                                &self.secp,
                                offer_params,
                                accept_params,
                                &own_sk,
                                &counter_sk,
                                &counter_revocation_sk,
                                &tx,
                                &self.wallet.get_new_address()?,
                                0,
                                fee_rate_per_vb,
                            )?
                        }
                        RevokedTxType::Settle => {
                            dlc::channel::create_and_sign_punish_settle_transaction(
                                &self.secp,
                                offer_params,
                                accept_params,
                                &own_sk,
                                &counter_sk,
                                &counter_revocation_sk,
                                &tx,
                                &self.wallet.get_new_address()?,
                                CET_NSEQUENCE,
                                0,
                                fee_rate_per_vb,
                                is_offer,
                            )?
                        }
                        RevokedTxType::Split => {
                            dlc::channel::sub_channel::create_and_sign_punish_split_transaction(
                                &self.secp,
                                offer_params,
                                accept_params,
                                &own_sk,
                                &counter_sk,
                                &counter_revocation_sk,
                                &tx,
                                &self.wallet.get_new_address()?,
                                0,
                                fee_rate_per_vb,
                            )?
                        }
                    };

                    self.blockchain.send_transaction(&signed_tx)?;

                    let closed_channel = Channel::ClosedPunished(ClosedPunishedChannel {
                        counter_party: signed_channel.counter_party,
                        temporary_channel_id: signed_channel.temporary_channel_id,
                        channel_id: signed_channel.channel_id,
                        punish_txid: signed_tx.txid(),
                        reference_id: None
                    });

                    //TODO(tibo): should probably make sure the tx is confirmed somewhere before
                    //stop watching the cheating tx.
                    self.chain_monitor
                        .lock()
                        .unwrap()
                        .cleanup_channel(signed_channel.channel_id);
                    self.store.upsert_channel(closed_channel, None)?;
                    true
                }
                TxType::CollaborativeClose => {
                    let counter_payout = get_signed_channel_state!(
                            signed_channel,
                            CollaborativeCloseOffered,
                            counter_payout
                        )?;
                    if let Some(SignedChannelState::Established {
                        signed_contract_id, ..
                    }) = signed_channel.roll_back_state
                    {
                        let closed_contract = self.get_collaboratively_closed_contract(
                            &signed_contract_id,
                            *counter_payout,
                            false,
                        )?;
                        self.store
                            .update_contract(&Contract::Closed(closed_contract))?;
                    }

                    let closed_channel = Channel::CollaborativelyClosed(ClosedChannel {
                        counter_party: signed_channel.counter_party,
                        temporary_channel_id: signed_channel.temporary_channel_id,
                        channel_id: signed_channel.channel_id,
                        reference_id: signed_channel.reference_id,
                        closing_txid: tx.txid()
                    });
                    self.chain_monitor
                        .lock()
                        .unwrap()
                        .cleanup_channel(signed_channel.channel_id);
                    self.store.upsert_channel(closed_channel, None)?;
                    true
                }
                TxType::SettleTx => {
                    // TODO(tibo): should only considered closed after some confirmations.
                    // Ideally should save previous state, and maybe restore in
                    // case of reorg, though if the counter party has sent the
                    // tx to close the channel it is unlikely that the tx will
                    // not be part of a future block.

                    let is_settle_offer = {
                        let chain_monitor = self.chain_monitor.lock().unwrap();
                        chain_monitor.did_we_offer_last_channel_settlement(&signed_channel.channel_id)
                    };

                    let is_settle_offer = match is_settle_offer {
                        Some(is_settle_offer) => is_settle_offer,
                        None => {
                            log::error!("Cannot force close settled channel without knowledge of who offered settlement");
                            continue;
                        },
                    };

                    let mut state = SignedChannelState::SettledClosing {
                        settle_transaction: tx.clone(),
                        is_offer: is_settle_offer,
                        is_initiator: false,
                    };
                    std::mem::swap(&mut signed_channel.state, &mut state);

                    signed_channel.roll_back_state = Some(state);

                    self.store
                        .upsert_channel(Channel::Signed(signed_channel), None)?;

                    false
                }
                TxType::SettleTx2 { is_offer } => {
                    // TODO(tibo): should only considered closed after some confirmations.
                    // Ideally should save previous state, and maybe restore in
                    // case of reorg, though if the counter party has sent the
                    // tx to close the channel it is unlikely that the tx will
                    // not be part of a future block.

                    let mut state = SignedChannelState::SettledClosing {
                        settle_transaction: tx.clone(),
                        is_offer,
                        is_initiator: false,
                    };
                    std::mem::swap(&mut signed_channel.state, &mut state);

                    signed_channel.roll_back_state = Some(state);

                    self.store
                        .upsert_channel(Channel::Signed(signed_channel), None)?;

                    false
                }
                TxType::Cet => {
                    let contract_id = signed_channel.get_contract_id();
                    let closed_channel = {
                        match &signed_channel.state {
                            SignedChannelState::Closing { is_initiator, .. } => {
                                if *is_initiator {
                                    Channel::Closed(ClosedChannel {
                                        counter_party: signed_channel.counter_party,
                                        temporary_channel_id: signed_channel.temporary_channel_id,
                                        channel_id: signed_channel.channel_id,
                                        reference_id: signed_channel.reference_id,
                                        closing_txid: tx.txid()
                                    })
                                } else {
                                    Channel::CounterClosed(ClosedChannel {
                                        counter_party: signed_channel.counter_party,
                                        temporary_channel_id: signed_channel.temporary_channel_id,
                                        channel_id: signed_channel.channel_id,
                                        reference_id: signed_channel.reference_id,
                                        closing_txid: tx.txid()
                                    })
                                }
                            }
                            _ => {
                                error!("Saw spending of buffer transaction without being in closing state");
                                Channel::Closed(ClosedChannel {
                                    counter_party: signed_channel.counter_party,
                                    temporary_channel_id: signed_channel.temporary_channel_id,
                                    channel_id: signed_channel.channel_id,
                                    reference_id: None,
                                    closing_txid: tx.txid()
                                })
                            }
                        }
                    };

                    self.chain_monitor
                        .lock()
                        .unwrap()
                        .cleanup_channel(signed_channel.channel_id);

                    let pre_closed_contract = contract_id
                        .map(|contract_id| {
                            self.store.get_contract(&contract_id).map(|contract| {
                                contract.map(|contract| match contract {
                                    Contract::Confirmed(signed_contract) => {
                                        Some(Contract::PreClosed(PreClosedContract {
                                            signed_contract,
                                            attestations: None,
                                            signed_cet: tx.clone(),
                                        }))
                                    }
                                    _ => None,
                                })
                            })
                        })
                        .transpose()?
                        .flatten()
                        .flatten();

                    self.store
                        .upsert_channel(closed_channel, pre_closed_contract)?;

                    true
                }
                TxType::SplitTx => false,
            };

            if persist {
                self.store
                    .persist_chain_monitor(&self.chain_monitor.lock().unwrap())?;
            }
        }
        Ok(())
    }

    fn check_for_watched_tx(&self) -> Result<(), Error> {
        let confirmed_txs = self.chain_monitor.lock().unwrap().confirmed_txs();

        self.process_watched_txs(confirmed_txs)?;

        self.get_store()
            .persist_chain_monitor(&self.chain_monitor.lock().unwrap())?;

        Ok(())
    }

    pub(crate) fn force_close_sub_channel(
        &self,
        channel_id: &DlcChannelId,
        sub_channel: (SubChannel, &ClosingSubChannel)
    ) -> Result<(), Error> {
        let channel = get_channel_in_state!(self, channel_id, Signed, None as Option<PublicKey>)?;
        let is_initiator = sub_channel.1.is_initiator;
        self.force_close_channel_internal(channel, Some(sub_channel), is_initiator, None)
    }

    fn force_close_channel_internal(
        &self,
        mut channel: SignedChannel,
        sub_channel: Option<(SubChannel, &ClosingSubChannel)>,
        is_initiator: bool,
        reference_id: Option<ReferenceId>
    ) -> Result<(), Error> {
        match &channel.state {
            SignedChannelState::Established {
                counter_buffer_adaptor_signature,
                buffer_transaction,
                ..
            } => {
                warn!("Force closing established channel with id: {}", channel.channel_id.to_hex());

                let counter_buffer_adaptor_signature = *counter_buffer_adaptor_signature;
                let buffer_transaction = buffer_transaction.clone();
                self.initiate_unilateral_close_established_channel(
                    channel,
                    sub_channel,
                    is_initiator,
                    counter_buffer_adaptor_signature,
                    buffer_transaction,
                    reference_id
                )
            }
            SignedChannelState::RenewFinalized {
                buffer_transaction,
                offer_buffer_adaptor_signature,
                ..
            } => {
                warn!("Force closing renew finalized channel with id: {}", channel.channel_id.to_hex());

                let offer_buffer_adaptor_signature = *offer_buffer_adaptor_signature;
                let buffer_transaction = buffer_transaction.clone();
                self.initiate_unilateral_close_established_channel(
                    channel,
                    sub_channel,
                    is_initiator,
                    offer_buffer_adaptor_signature,
                    buffer_transaction,
                    reference_id
                )
            }
            SignedChannelState::Settled { .. } => {
                warn!("Force closing settled channel with id: {}", channel.channel_id.to_hex());

                self.initiate_unilateral_close_settled_channel(channel, sub_channel, is_initiator, reference_id)
            }
            SignedChannelState::SettledOffered { .. }
            | SignedChannelState::SettledReceived { .. }
            | SignedChannelState::SettledAccepted { .. }
            | SignedChannelState::SettledConfirmed { .. }
            | SignedChannelState::RenewOffered { .. }
            | SignedChannelState::RenewAccepted { .. }
            | SignedChannelState::RenewConfirmed { .. }
            | SignedChannelState::CollaborativeCloseOffered { .. } => {
                channel.state = channel
                    .roll_back_state
                    .take()
                    .expect("to have a rollback state");
                self.force_close_channel_internal(channel, sub_channel, is_initiator, reference_id)
            }
            SignedChannelState::Closing { .. } | SignedChannelState::SettledClosing { .. } => Err(Error::InvalidState(
                "Channel is already closing.".to_string(),
            )),
        }
    }

    /// Initiate the unilateral closing of a channel that has been established.
    fn initiate_unilateral_close_established_channel(
        &self,
        mut signed_channel: SignedChannel,
        sub_channel: Option<(SubChannel, &ClosingSubChannel)>,
        is_initiator: bool,
        buffer_adaptor_signature: EcdsaAdaptorSignature,
        buffer_transaction: Transaction,
        reference_id: Option<ReferenceId>
    ) -> Result<(), Error> {
        crate::channel_updater::initiate_unilateral_close_established_channel(
            &self.secp,
            &mut signed_channel,
            buffer_adaptor_signature,
            buffer_transaction,
            &self.wallet,
            sub_channel,
            is_initiator,
            reference_id
        )?;

        let buffer_transaction =
            get_signed_channel_state!(signed_channel, Closing, ref buffer_transaction)?;

        if self
            .blockchain
            .get_transaction_confirmations(&buffer_transaction.txid())
            .unwrap_or(0)
            == 0
        {
            self.blockchain.send_transaction(buffer_transaction)?;
        }

        self.chain_monitor
            .lock()
            .unwrap()
            .remove_tx(&buffer_transaction.txid());

        self.store
            .upsert_channel(Channel::Signed(signed_channel), None)?;

        self.store
            .persist_chain_monitor(&self.chain_monitor.lock().unwrap())?;

        Ok(())
    }

    /// Unilaterally close a channel that has been settled.
    fn initiate_unilateral_close_settled_channel(
        &self,
        mut signed_channel: SignedChannel,
        sub_channel: Option<(SubChannel, &ClosingSubChannel)>,
        is_initiator: bool,
        reference_id: Option<ReferenceId>
    ) -> Result<(), Error> {
        let is_settle_offer = {
            let chain_monitor = self.chain_monitor.lock().unwrap();
            chain_monitor.did_we_offer_last_channel_settlement(&signed_channel.channel_id)
        }.ok_or_else(
            || Error::InvalidState(
                "Cannot force close settled channel without knowledge of who offered settlement"
                    .to_string()
            )
        )?;

        crate::channel_updater::initiate_unilateral_close_settled_channel(
            &self.secp,
            &mut signed_channel,
            &self.wallet,
            sub_channel,
            is_settle_offer,
            is_initiator,
            reference_id,
        )?;

        let settle_tx =
            get_signed_channel_state!(signed_channel, SettledClosing, ref settle_transaction)?;

        if self
            .blockchain
            .get_transaction_confirmations(&settle_tx.txid())
            .unwrap_or(0)
            == 0
        {
            self.blockchain.send_transaction(settle_tx)?;
        }

        self.chain_monitor
            .lock()
            .unwrap()
            .cleanup_channel(signed_channel.channel_id);

        self.store.upsert_channel(Channel::Signed(signed_channel), None)?;

        Ok(())
    }

    /// Function used to mark a dlc channel as closed when the parent sub channel was
    /// collaboratively closed.
    pub(crate) fn get_closed_sub_dlc_channel(
        &self,
        channel_id: DlcChannelId,
        own_balance: u64,
    ) -> Result<(Channel, Option<Contract>), Error> {
        let channel = get_channel_in_state!(self, &channel_id, Signed, None::<PublicKey>)?;

        let contract = if let Some(contract_id) = channel.get_contract_id() {
            Some(Contract::Closed(self.get_collaboratively_closed_contract(
                &contract_id,
                own_balance,
                true,
            )?))
        } else {
            None
        };

        let closed_channel = Channel::CollaborativelyClosed(ClosedChannel {
            counter_party: channel.counter_party,
            temporary_channel_id: channel.temporary_channel_id,
            channel_id,
            reference_id: None,
            // TODO(holzeis): Ignoring closing txid on dlc channels for sub channels
            closing_txid: Txid::all_zeros()
        });

        Ok((closed_channel, contract))
    }

    fn get_collaboratively_closed_contract(
        &self,
        contract_id: &ContractId,
        payout: u64,
        is_own_payout: bool,
    ) -> Result<ClosedContract, Error> {
        let contract = get_contract_in_state!(self, contract_id, Confirmed, None::<PublicKey>)?;
        let own_collateral = if contract.accepted_contract.offered_contract.is_offer_party {
            contract
                .accepted_contract
                .offered_contract
                .offer_params
                .collateral
        } else {
            contract.accepted_contract.accept_params.collateral
        };
        let own_payout = if is_own_payout {
            payout
        } else {
            contract.accepted_contract.offered_contract.total_collateral - payout
        };
        let pnl = own_payout as i64 - own_collateral as i64;
        Ok(ClosedContract {
            attestations: None,
            signed_cet: None,
            contract_id: *contract_id,
            temporary_contract_id: contract.accepted_contract.offered_contract.id,
            counter_party_id: contract.accepted_contract.offered_contract.counter_party,
            pnl,
        })
    }
}

#[cfg(test)]
mod test {
    use dlc_messages::{ChannelMessage, Message, OnChainMessage};
    use mocks::{
        dlc_manager::{manager::Manager, Oracle},
        memory_storage_provider::MemoryStorage,
        mock_blockchain::{MockBlockchain, MockBroadcaster},
        mock_oracle_provider::MockOracle,
        mock_time::MockTime,
        mock_wallet::MockWallet,
    };
    use secp256k1_zkp::PublicKey;
    use std::{collections::HashMap, rc::Rc};

    type TestManager = Manager<
        Rc<MockWallet>,
        Rc<MockBlockchain<Rc<MockBroadcaster>>>,
        Rc<MemoryStorage>,
        Rc<MockOracle>,
        Rc<MockTime>,
        Rc<MockBlockchain<Rc<MockBroadcaster>>>,
    >;

    fn get_manager() -> TestManager {
        let blockchain = Rc::new(MockBlockchain::new(Rc::new(MockBroadcaster {})));
        let store = Rc::new(MemoryStorage::new());
        let wallet = Rc::new(MockWallet::new(&blockchain, 100));

        let oracle_list = (0..5).map(|_| MockOracle::new()).collect::<Vec<_>>();
        let oracles: HashMap<bitcoin::XOnlyPublicKey, _> = oracle_list
            .into_iter()
            .map(|x| (x.get_public_key(), Rc::new(x)))
            .collect();
        let time = Rc::new(MockTime {});

        mocks::mock_time::set_time(0);

        Manager::new(wallet, blockchain.clone(), store, oracles, time, blockchain).unwrap()
    }

    fn pubkey() -> PublicKey {
        "0218845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166"
            .parse()
            .unwrap()
    }

    #[test]
    fn reject_offer_with_existing_contract_id() {
        let offer_message = Message::OnChain(OnChainMessage::Offer(
            serde_json::from_str(include_str!("../test_inputs/offer_contract.json")).unwrap(),
        ));

        let manager = get_manager();

        manager
            .on_dlc_message(&offer_message, pubkey())
            .expect("To accept the first offer message");

        manager
            .on_dlc_message(&offer_message, pubkey())
            .expect_err("To reject the second offer message");
    }

    #[test]
    fn reject_channel_offer_with_existing_channel_id() {
        let offer_message = Message::Channel(ChannelMessage::Offer(
            serde_json::from_str(include_str!("../test_inputs/offer_channel.json")).unwrap(),
        ));

        let manager = get_manager();

        manager
            .on_dlc_message(&offer_message, pubkey())
            .expect("To accept the first offer message");

        manager
            .on_dlc_message(&offer_message, pubkey())
            .expect_err("To reject the second offer message");
    }
}
