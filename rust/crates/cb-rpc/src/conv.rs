//! capnp <-> Rust mirrors.

use cb_block::{Block, BlockHeader};
use cb_consensus::Vote;
use cb_schema::block_capnp;
use cb_schema::rpc_capnp;
use cb_schema::tx_capnp;
use cb_schema::types_capnp;
use cb_state::{AccountState, MarketConfig};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{
    Address, AssetId, DerivId, FundingIndex, FutureMarket, LiquidationEntry, MarginAccount,
    MarkPrice, MarketId, OraclePrice, PerpMarket, Position, Side, Signed, Tif, H256,
};

// ---------- length caps ----------
//
// Every variable-length field a client can put in a request is capped here,
// at decode, before it is copied into a Rust value. The Cap'n Proto reader
// hands out a view into the message, so a check on `len()` costs nothing and
// an oversized field is refused before it is allocated, hashed, signed over,
// kept in the mempool, gossiped and written into a block.

/// Most bytes in a free-text reference to another system: a deposit's or a
/// settled withdrawal's external transaction id, a withdrawal's destination
/// address. The longest honest values are around a hundred bytes (a 0x-hex
/// hash is 66, a bech32 address at most 90, a base58 Solana signature 88).
pub const MAX_EXTERNAL_REF_LEN: usize = 256;

/// A vote's signature is a 65-byte recoverable secp256k1 signature. Anything
/// longer is refused at decode; shorter is left to recovery, which refuses it
/// with the more specific `malformedSig`.
pub const MAX_VOTE_SIG_LEN: usize = 65;

/// Most bytes of a block header's finality proof or sequencer signature a
/// client accepts from a server. A four-validator proof is under 2 KB; this
/// leaves room for a large set without letting a hostile server hand back
/// megabytes per header.
pub const MAX_HEADER_BLOB_LEN: usize = 256 * 1024;

fn capped_text(r: capnp::text::Reader<'_>, field: &str, max: usize) -> capnp::Result<String> {
    if r.len() > max {
        return Err(capnp::Error::failed(format!(
            "{field} is {} bytes; the limit is {max}",
            r.len()
        )));
    }
    Ok(r.to_string()?)
}

fn capped_data(r: &[u8], field: &str, max: usize) -> capnp::Result<Vec<u8>> {
    if r.len() > max {
        return Err(capnp::Error::failed(format!(
            "{field} is {} bytes; the limit is {max}",
            r.len()
        )));
    }
    Ok(r.to_vec())
}

// ---------- primitives ----------

pub fn address_from_capnp(r: types_capnp::address::Reader<'_>) -> capnp::Result<Address> {
    let bytes = r.get_bytes()?;
    if bytes.len() != 20 {
        return Err(capnp::Error::failed("address must be 20 bytes".into()));
    }
    let mut a = [0u8; 20];
    a.copy_from_slice(bytes);
    Ok(Address(a))
}

pub fn address_to_capnp(mut b: types_capnp::address::Builder<'_>, a: &Address) {
    b.set_bytes(&a.0);
}

pub fn h256_from_capnp(r: types_capnp::h256::Reader<'_>) -> capnp::Result<H256> {
    let bytes = r.get_bytes()?;
    if bytes.len() != 32 {
        return Err(capnp::Error::failed("h256 must be 32 bytes".into()));
    }
    let mut h = [0u8; 32];
    h.copy_from_slice(bytes);
    Ok(H256(h))
}

pub fn h256_to_capnp(mut b: types_capnp::h256::Builder<'_>, h: &H256) {
    b.set_bytes(&h.0);
}

pub fn u128_from_capnp(r: types_capnp::u128::Reader<'_>) -> u128 {
    ((r.get_hi() as u128) << 64) | (r.get_lo() as u128)
}

pub fn u128_to_capnp(mut b: types_capnp::u128::Builder<'_>, v: u128) {
    b.set_hi((v >> 64) as u64);
    b.set_lo(v as u64);
}

/// Cap'n Proto `AssetId` -> `cb_types::AssetId`.
///
/// Written as an explicit, exhaustive match rather than an ordinal cast so that
/// appending an enumerant to `types.capnp` fails to compile here until it is
/// mapped. `asset_ordinals_match_capnp` pins that the two numberings agree.
pub fn asset_from_capnp(a: types_capnp::AssetId) -> AssetId {
    match a {
        types_capnp::AssetId::Btc => AssetId::Btc,
        types_capnp::AssetId::Eth => AssetId::Eth,
        types_capnp::AssetId::Usd => AssetId::Usd,
        types_capnp::AssetId::Sol => AssetId::Sol,
        types_capnp::AssetId::Aapl => AssetId::Aapl,
        types_capnp::AssetId::Tsla => AssetId::Tsla,
        types_capnp::AssetId::Nvda => AssetId::Nvda,
        types_capnp::AssetId::Spy => AssetId::Spy,
    }
}

/// `cb_types::AssetId` -> Cap'n Proto `AssetId`.
pub fn asset_to_capnp(a: AssetId) -> types_capnp::AssetId {
    match a {
        AssetId::Btc => types_capnp::AssetId::Btc,
        AssetId::Eth => types_capnp::AssetId::Eth,
        AssetId::Usd => types_capnp::AssetId::Usd,
        AssetId::Sol => types_capnp::AssetId::Sol,
        AssetId::Aapl => types_capnp::AssetId::Aapl,
        AssetId::Tsla => types_capnp::AssetId::Tsla,
        AssetId::Nvda => types_capnp::AssetId::Nvda,
        AssetId::Spy => types_capnp::AssetId::Spy,
    }
}

/// Cap'n Proto `MarketId` -> `cb_types::MarketId`.
pub fn market_from_capnp(m: types_capnp::MarketId) -> MarketId {
    match m {
        types_capnp::MarketId::BtcUsd => MarketId::BtcUsd,
        types_capnp::MarketId::EthUsd => MarketId::EthUsd,
        types_capnp::MarketId::SolUsd => MarketId::SolUsd,
        types_capnp::MarketId::AaplUsd => MarketId::AaplUsd,
        types_capnp::MarketId::TslaUsd => MarketId::TslaUsd,
        types_capnp::MarketId::NvdaUsd => MarketId::NvdaUsd,
        types_capnp::MarketId::SpyUsd => MarketId::SpyUsd,
    }
}

/// `cb_types::MarketId` -> Cap'n Proto `MarketId`.
pub fn market_to_capnp(m: MarketId) -> types_capnp::MarketId {
    match m {
        MarketId::BtcUsd => types_capnp::MarketId::BtcUsd,
        MarketId::EthUsd => types_capnp::MarketId::EthUsd,
        MarketId::SolUsd => types_capnp::MarketId::SolUsd,
        MarketId::AaplUsd => types_capnp::MarketId::AaplUsd,
        MarketId::TslaUsd => types_capnp::MarketId::TslaUsd,
        MarketId::NvdaUsd => types_capnp::MarketId::NvdaUsd,
        MarketId::SpyUsd => types_capnp::MarketId::SpyUsd,
    }
}

pub fn side_from_capnp(s: types_capnp::Side) -> Side {
    match s {
        types_capnp::Side::Buy => Side::Buy,
        types_capnp::Side::Sell => Side::Sell,
    }
}

pub fn side_to_capnp(s: Side) -> types_capnp::Side {
    match s {
        Side::Buy => types_capnp::Side::Buy,
        Side::Sell => types_capnp::Side::Sell,
    }
}

pub fn tif_from_capnp(t: types_capnp::Tif) -> Tif {
    match t {
        types_capnp::Tif::Gtc => Tif::Gtc,
        types_capnp::Tif::Ioc => Tif::Ioc,
        types_capnp::Tif::Fok => Tif::Fok,
    }
}

pub fn tif_to_capnp(t: Tif) -> types_capnp::Tif {
    match t {
        Tif::Gtc => types_capnp::Tif::Gtc,
        Tif::Ioc => types_capnp::Tif::Ioc,
        Tif::Fok => types_capnp::Tif::Fok,
    }
}

pub fn deriv_id_from_capnp(r: types_capnp::deriv_id::Reader<'_>) -> capnp::Result<DerivId> {
    let bytes = r.get_bytes()?;
    if bytes.len() != 4 {
        return Err(capnp::Error::failed("DerivId must be 4 bytes".into()));
    }
    let mut b = [0u8; 4];
    b.copy_from_slice(bytes);
    DerivId::from_bytes(b).map_err(|e| capnp::Error::failed(e.to_string()))
}

pub fn deriv_id_to_capnp(mut b: types_capnp::deriv_id::Builder<'_>, d: &DerivId) {
    b.set_bytes(&d.0);
}

// ---------- Transaction ----------

pub fn transaction_from_capnp(r: tx_capnp::transaction::Reader<'_>) -> capnp::Result<Transaction> {
    use tx_capnp::transaction::body::Which;
    let body = match r.get_body().which()? {
        Which::Transfer(t) => {
            let t = t?;
            TxBody::Transfer {
                to: address_from_capnp(t.get_to()?)?,
                asset: asset_from_capnp(t.get_asset()?),
                amount: u128_from_capnp(t.get_amount()?),
            }
        }
        Which::Deposit(d) => {
            let d = d?;
            TxBody::Deposit {
                user: address_from_capnp(d.get_user()?)?,
                asset: asset_from_capnp(d.get_asset()?),
                amount: u128_from_capnp(d.get_amount()?),
                external_tx_ref: capped_text(
                    d.get_external_tx_ref()?,
                    "externalTxRef",
                    MAX_EXTERNAL_REF_LEN,
                )?,
            }
        }
        Which::Withdraw(w) => {
            let w = w?;
            TxBody::Withdraw {
                asset: asset_from_capnp(w.get_asset()?),
                amount: u128_from_capnp(w.get_amount()?),
                external_address: capped_text(
                    w.get_external_address()?,
                    "externalAddress",
                    MAX_EXTERNAL_REF_LEN,
                )?,
            }
        }
        Which::WithdrawSettled(s) => {
            let s = s?;
            TxBody::WithdrawSettled {
                withdraw_id: h256_from_capnp(s.get_withdraw_id()?)?,
                external_tx_ref: capped_text(
                    s.get_external_tx_ref()?,
                    "externalTxRef",
                    MAX_EXTERNAL_REF_LEN,
                )?,
            }
        }
        Which::PlaceOrder(p) => {
            let p = p?;
            TxBody::PlaceOrder {
                market: market_from_capnp(p.get_market()?),
                side: side_from_capnp(p.get_side()?),
                price: u128_from_capnp(p.get_price()?),
                qty: u128_from_capnp(p.get_qty()?),
                tif: tif_from_capnp(p.get_tif()?),
                post_only: p.get_post_only(),
                client_order_id: h256_from_capnp(p.get_client_order_id()?)?,
            }
        }
        Which::CancelOrder(c) => {
            let c = c?;
            TxBody::CancelOrder {
                order_id: h256_from_capnp(c.get_order_id()?)?,
            }
        }
        Which::BatchCancel(b) => {
            let b = b?;
            TxBody::BatchCancel {
                market: market_from_capnp(b.get_market()?),
            }
        }
        // ---- derivatives ----
        Which::PlacePerpOrder(p) => {
            let p = p?;
            TxBody::PlacePerpOrder {
                market: deriv_id_from_capnp(p.get_market()?)?,
                side: side_from_capnp(p.get_side()?),
                price_q8: u128_from_capnp(p.get_price_q8()?),
                qty: u128_from_capnp(p.get_qty()?),
                tif: tif_from_capnp(p.get_tif()?),
                post_only: p.get_post_only(),
                reduce_only: p.get_reduce_only(),
                client_order_id: h256_from_capnp(p.get_client_order_id()?)?,
            }
        }
        Which::CancelPerpOrder(c) => {
            let c = c?;
            TxBody::CancelPerpOrder {
                order_id: h256_from_capnp(c.get_order_id()?)?,
            }
        }
        Which::OpenPosition(o) => {
            let o = o?;
            TxBody::OpenPosition {
                market: deriv_id_from_capnp(o.get_market()?)?,
                side: side_from_capnp(o.get_side()?),
                qty: u128_from_capnp(o.get_qty()?),
                max_slippage_bps: o.get_max_slippage_bps(),
            }
        }
        Which::ClosePosition(c) => {
            let c = c?;
            TxBody::ClosePosition {
                market: deriv_id_from_capnp(c.get_market()?)?,
                qty: u128_from_capnp(c.get_qty()?),
                max_slippage_bps: c.get_max_slippage_bps(),
            }
        }
        Which::AdjustMargin(a) => {
            let a = a?;
            TxBody::AdjustMargin {
                market: deriv_id_from_capnp(a.get_market()?)?,
                is_topup: a.get_is_topup(),
                amount: u128_from_capnp(a.get_amount()?),
            }
        }
        Which::LiquidationTrigger(l) => {
            let l = l?;
            TxBody::LiquidationTrigger {
                owner: address_from_capnp(l.get_owner()?)?,
                market: deriv_id_from_capnp(l.get_market()?)?,
            }
        }
        Which::OracleUpdate(u) => {
            let u = u?;
            TxBody::OracleUpdate {
                market: deriv_id_from_capnp(u.get_market()?)?,
                px_q8: u128_from_capnp(u.get_px_q8()?),
                submitted_height: u.get_submitted_height(),
            }
        }
        Which::FundingTick(t) => {
            let t = t?;
            TxBody::FundingTick {
                market: deriv_id_from_capnp(t.get_market()?)?,
            }
        }
        Which::FuturesSettle(s) => {
            let s = s?;
            TxBody::FuturesSettle {
                market: deriv_id_from_capnp(s.get_market()?)?,
            }
        }
    };
    Ok(Transaction {
        nonce: r.get_nonce(),
        chain_id: r.get_chain_id(),
        body,
    })
}

pub fn transaction_to_capnp(mut b: tx_capnp::transaction::Builder<'_>, tx: &Transaction) {
    b.set_nonce(tx.nonce);
    b.set_chain_id(tx.chain_id);
    let body = b.init_body();
    match &tx.body {
        TxBody::Transfer { to, asset, amount } => {
            let mut t = body.init_transfer();
            address_to_capnp(t.reborrow().init_to(), to);
            t.set_asset(asset_to_capnp(*asset));
            u128_to_capnp(t.init_amount(), *amount);
        }
        TxBody::Deposit {
            user,
            asset,
            amount,
            external_tx_ref,
        } => {
            let mut d = body.init_deposit();
            address_to_capnp(d.reborrow().init_user(), user);
            d.set_asset(asset_to_capnp(*asset));
            u128_to_capnp(d.reborrow().init_amount(), *amount);
            d.set_external_tx_ref(external_tx_ref.as_str());
        }
        TxBody::Withdraw {
            asset,
            amount,
            external_address,
        } => {
            let mut w = body.init_withdraw();
            w.set_asset(asset_to_capnp(*asset));
            u128_to_capnp(w.reborrow().init_amount(), *amount);
            w.set_external_address(external_address.as_str());
        }
        TxBody::WithdrawSettled {
            withdraw_id,
            external_tx_ref,
        } => {
            let mut s = body.init_withdraw_settled();
            h256_to_capnp(s.reborrow().init_withdraw_id(), withdraw_id);
            s.set_external_tx_ref(external_tx_ref.as_str());
        }
        TxBody::PlaceOrder {
            market,
            side,
            price,
            qty,
            tif,
            post_only,
            client_order_id,
        } => {
            let mut p = body.init_place_order();
            p.set_market(market_to_capnp(*market));
            p.set_side(side_to_capnp(*side));
            u128_to_capnp(p.reborrow().init_price(), *price);
            u128_to_capnp(p.reborrow().init_qty(), *qty);
            p.set_tif(tif_to_capnp(*tif));
            p.set_post_only(*post_only);
            h256_to_capnp(p.init_client_order_id(), client_order_id);
        }
        TxBody::CancelOrder { order_id } => {
            let c = body.init_cancel_order();
            h256_to_capnp(c.init_order_id(), order_id);
        }
        TxBody::BatchCancel { market } => {
            let mut bc = body.init_batch_cancel();
            bc.set_market(market_to_capnp(*market));
        }
        // ---- derivatives ----
        TxBody::PlacePerpOrder {
            market,
            side,
            price_q8,
            qty,
            tif,
            post_only,
            reduce_only,
            client_order_id,
        } => {
            let mut p = body.init_place_perp_order();
            deriv_id_to_capnp(p.reborrow().init_market(), market);
            p.set_side(side_to_capnp(*side));
            u128_to_capnp(p.reborrow().init_price_q8(), *price_q8);
            u128_to_capnp(p.reborrow().init_qty(), *qty);
            p.set_tif(tif_to_capnp(*tif));
            p.set_post_only(*post_only);
            p.set_reduce_only(*reduce_only);
            h256_to_capnp(p.init_client_order_id(), client_order_id);
        }
        TxBody::CancelPerpOrder { order_id } => {
            let c = body.init_cancel_perp_order();
            h256_to_capnp(c.init_order_id(), order_id);
        }
        TxBody::OpenPosition {
            market,
            side,
            qty,
            max_slippage_bps,
        } => {
            let mut o = body.init_open_position();
            deriv_id_to_capnp(o.reborrow().init_market(), market);
            o.set_side(side_to_capnp(*side));
            u128_to_capnp(o.reborrow().init_qty(), *qty);
            o.set_max_slippage_bps(*max_slippage_bps);
        }
        TxBody::ClosePosition {
            market,
            qty,
            max_slippage_bps,
        } => {
            let mut c = body.init_close_position();
            deriv_id_to_capnp(c.reborrow().init_market(), market);
            u128_to_capnp(c.reborrow().init_qty(), *qty);
            c.set_max_slippage_bps(*max_slippage_bps);
        }
        TxBody::AdjustMargin {
            market,
            is_topup,
            amount,
        } => {
            let mut a = body.init_adjust_margin();
            deriv_id_to_capnp(a.reborrow().init_market(), market);
            a.set_is_topup(*is_topup);
            u128_to_capnp(a.init_amount(), *amount);
        }
        TxBody::LiquidationTrigger { owner, market } => {
            let mut l = body.init_liquidation_trigger();
            address_to_capnp(l.reborrow().init_owner(), owner);
            deriv_id_to_capnp(l.init_market(), market);
        }
        TxBody::OracleUpdate {
            market,
            px_q8,
            submitted_height,
        } => {
            let mut u = body.init_oracle_update();
            deriv_id_to_capnp(u.reborrow().init_market(), market);
            u128_to_capnp(u.reborrow().init_px_q8(), *px_q8);
            u.set_submitted_height(*submitted_height);
        }
        TxBody::FundingTick { market } => {
            let t = body.init_funding_tick();
            deriv_id_to_capnp(t.init_market(), market);
        }
        TxBody::FuturesSettle { market } => {
            let s = body.init_futures_settle();
            deriv_id_to_capnp(s.init_market(), market);
        }
    }
}

// ---------- SignedTx ----------

pub fn signed_tx_from_capnp(r: tx_capnp::signed_tx::Reader<'_>) -> capnp::Result<SignedTx> {
    let tx = transaction_from_capnp(r.get_tx()?)?;
    let sig = r.get_sig()?;
    let r_bytes = sig.get_r()?;
    let s_bytes = sig.get_s()?;
    if r_bytes.len() != 32 || s_bytes.len() != 32 {
        return Err(capnp::Error::failed("sig r/s must be 32 bytes".into()));
    }
    let mut sig_r = [0u8; 32];
    sig_r.copy_from_slice(r_bytes);
    let mut sig_s = [0u8; 32];
    sig_s.copy_from_slice(s_bytes);
    Ok(SignedTx {
        tx,
        sig_r,
        sig_s,
        sig_recovery: sig.get_recovery_id(),
    })
}

pub fn signed_tx_to_capnp(mut b: tx_capnp::signed_tx::Builder<'_>, s: &SignedTx) {
    transaction_to_capnp(b.reborrow().init_tx(), &s.tx);
    let mut sig = b.init_sig();
    sig.set_r(&s.sig_r);
    sig.set_s(&s.sig_s);
    sig.set_recovery_id(s.sig_recovery);
}

// ---------- Block ----------

pub fn block_to_capnp(mut b: block_capnp::block::Builder<'_>, blk: &Block) {
    let mut h = b.reborrow().init_header();
    h.set_height(blk.header.height);
    h256_to_capnp(h.reborrow().init_prev_hash(), &blk.header.prev_hash);
    h256_to_capnp(h.reborrow().init_tx_root(), &blk.header.tx_root);
    h256_to_capnp(h.reborrow().init_state_root(), &blk.header.state_root);
    h.set_timestamp(blk.header.timestamp);
    h.set_sequencer_sig(&blk.header.sequencer_sig);
    h.set_view(blk.header.view);
    h.set_parent_qc(&blk.header.parent_qc);

    let mut txs = b.reborrow().init_txs(blk.txs.len() as u32);
    for (i, s) in blk.txs.iter().enumerate() {
        signed_tx_to_capnp(txs.reborrow().get(i as u32), s);
    }
    let mut senders = b.reborrow().init_senders(blk.senders.len() as u32);
    for (i, a) in blk.senders.iter().enumerate() {
        address_to_capnp(senders.reborrow().get(i as u32), a);
    }
    let mut applied = b.init_applied(blk.applied.len() as u32);
    for (i, ok) in blk.applied.iter().enumerate() {
        applied.set(i as u32, *ok);
    }
}

pub fn block_from_capnp(r: block_capnp::block::Reader<'_>) -> capnp::Result<Block> {
    let h = r.get_header()?;
    let header = BlockHeader {
        height: h.get_height(),
        prev_hash: h256_from_capnp(h.get_prev_hash()?)?,
        tx_root: h256_from_capnp(h.get_tx_root()?)?,
        state_root: h256_from_capnp(h.get_state_root()?)?,
        timestamp: h.get_timestamp(),
        view: h.get_view(),
        parent_qc: capped_data(h.get_parent_qc()?, "parentQc", MAX_HEADER_BLOB_LEN)?,
        sequencer_sig: capped_data(h.get_sequencer_sig()?, "sequencerSig", MAX_HEADER_BLOB_LEN)?,
    };
    let mut txs = Vec::new();
    let txs_r = r.get_txs()?;
    for i in 0..txs_r.len() {
        txs.push(signed_tx_from_capnp(txs_r.get(i))?);
    }
    let mut senders: Vec<Address> = Vec::new();
    if r.has_senders() {
        let s_r = r.get_senders()?;
        for i in 0..s_r.len() {
            senders.push(address_from_capnp(s_r.get(i))?);
        }
    }
    let mut applied: Vec<bool> = Vec::new();
    if r.has_applied() {
        let a_r = r.get_applied()?;
        applied.reserve(a_r.len() as usize);
        for i in 0..a_r.len() {
            applied.push(a_r.get(i));
        }
    }
    Ok(Block {
        header,
        txs,
        senders,
        applied,
    })
}

// ---------- AccountState ----------

/// Serialise an `AccountState` into its RPC shape.
///
/// Fails rather than guesses if a balance row carries an asset ordinal this
/// build does not know. Such a row cannot be produced by any write path — every
/// mutation goes through a typed `AssetId` — so reaching the error means either
/// state corruption or a node running older code than the chain it is reading.
/// Both are cases where reporting the balance under the wrong asset would be
/// far worse than returning an error.
pub fn account_state_to_capnp(
    mut b: rpc_capnp::account_state::Builder<'_>,
    s: &AccountState,
) -> capnp::Result<()> {
    b.set_nonce(s.nonce);
    let mut bals = b.reborrow().init_balances(s.balances.len() as u32);
    for (i, (k, v)) in s.balances.iter().enumerate() {
        let mut entry = bals.reborrow().get(i as u32);
        entry.set_asset(asset_byte_to_capnp(*k)?);
        u128_to_capnp(entry.init_amount(), *v);
    }
    let mut locks = b.init_locked(s.locked.len() as u32);
    for (i, (k, v)) in s.locked.iter().enumerate() {
        let mut entry = locks.reborrow().get(i as u32);
        entry.set_asset(asset_byte_to_capnp(*k)?);
        u128_to_capnp(entry.init_amount(), *v);
    }
    Ok(())
}

/// Map a raw `AccountState` map key (an `AssetId::as_u8()` ordinal) to the wire
/// enum. Routed through `AssetId::from_u8` so there is exactly one place that
/// decides what an ordinal means, and so an unknown ordinal is an error instead
/// of silently becoming USD.
fn asset_byte_to_capnp(b: u8) -> capnp::Result<types_capnp::AssetId> {
    AssetId::from_u8(b)
        .map(asset_to_capnp)
        .map_err(|e| capnp::Error::failed(format!("account balance row: {e}")))
}

// ---------- Vote ----------

pub fn vote_to_capnp(mut b: rpc_capnp::vote::Builder<'_>, v: &Vote) {
    b.set_view(v.view);
    b.set_height(v.height);
    h256_to_capnp(b.reborrow().init_block_hash(), &v.block_hash);
    address_to_capnp(b.reborrow().init_validator(), &v.validator);
    b.set_sig(&v.sig);
}

pub fn vote_from_capnp(r: rpc_capnp::vote::Reader<'_>) -> capnp::Result<Vote> {
    Ok(Vote {
        view: r.get_view(),
        height: r.get_height(),
        block_hash: h256_from_capnp(r.get_block_hash()?)?,
        validator: address_from_capnp(r.get_validator()?)?,
        sig: capped_data(r.get_sig()?, "vote sig", MAX_VOTE_SIG_LEN)?,
    })
}

// ---------- Derivatives (M6) ----------
//
// Each helper mirrors the corresponding type in `cb-types::derivatives`. The
// capnp wire form follows that crate's bincode shape exactly except where the
// schema is more explicit (e.g. `Option<u128>` becomes `(has, value)` so the
// canonical bytes don't depend on `bincode::Options`).

pub fn signed_from_capnp(r: types_capnp::signed_i128::Reader<'_>) -> capnp::Result<Signed> {
    let abs = u128_from_capnp(r.get_abs()?);
    Signed::new(r.get_sign(), abs).map_err(|e| capnp::Error::failed(e.to_string()))
}

pub fn signed_to_capnp(mut b: types_capnp::signed_i128::Builder<'_>, s: &Signed) {
    b.set_sign(s.sign);
    u128_to_capnp(b.init_abs(), s.abs);
}

// ---- Position ----

pub fn position_to_capnp(mut b: rpc_capnp::position::Builder<'_>, p: &Position) {
    address_to_capnp(b.reborrow().init_owner(), &p.owner);
    deriv_id_to_capnp(b.reborrow().init_market(), &p.market);
    b.set_side(side_to_capnp(p.side));
    u128_to_capnp(b.reborrow().init_qty(), p.qty);
    signed_to_capnp(
        b.reborrow().init_entry_funding_index(),
        &p.entry_funding_index,
    );
    u128_to_capnp(b.reborrow().init_entry_px_q8(), p.entry_px_q8);
    b.set_margin_mode(p.margin_mode);
    u128_to_capnp(b.reborrow().init_isolated_margin(), p.isolated_margin);
    b.set_last_funding_height(p.last_funding_height);
}

pub fn position_from_capnp(r: rpc_capnp::position::Reader<'_>) -> capnp::Result<Position> {
    Ok(Position {
        owner: address_from_capnp(r.get_owner()?)?,
        market: deriv_id_from_capnp(r.get_market()?)?,
        side: side_from_capnp(r.get_side()?),
        qty: u128_from_capnp(r.get_qty()?),
        entry_funding_index: signed_from_capnp(r.get_entry_funding_index()?)?,
        entry_px_q8: u128_from_capnp(r.get_entry_px_q8()?),
        margin_mode: r.get_margin_mode(),
        isolated_margin: u128_from_capnp(r.get_isolated_margin()?),
        last_funding_height: r.get_last_funding_height(),
    })
}

// ---- MarginAccount ----

pub fn margin_account_to_capnp(mut b: rpc_capnp::margin_account::Builder<'_>, m: &MarginAccount) {
    address_to_capnp(b.reborrow().init_owner(), &m.owner);
    u128_to_capnp(b.reborrow().init_collateral(), m.collateral);
    signed_to_capnp(b.reborrow().init_unrealized_pnl(), &m.unrealized_pnl);
    u128_to_capnp(b.init_bad_debt(), m.bad_debt);
}

pub fn margin_account_from_capnp(
    r: rpc_capnp::margin_account::Reader<'_>,
) -> capnp::Result<MarginAccount> {
    Ok(MarginAccount {
        owner: address_from_capnp(r.get_owner()?)?,
        collateral: u128_from_capnp(r.get_collateral()?),
        unrealized_pnl: signed_from_capnp(r.get_unrealized_pnl()?)?,
        bad_debt: u128_from_capnp(r.get_bad_debt()?),
    })
}

// ---- FundingIndex ----

pub fn funding_index_to_capnp(mut b: rpc_capnp::funding_index::Builder<'_>, f: &FundingIndex) {
    deriv_id_to_capnp(b.reborrow().init_market(), &f.market);
    signed_to_capnp(b.reborrow().init_cumulative(), &f.cumulative);
    b.set_last_tick_height(f.last_tick_height);
    b.set_rate_q9(f.rate_q9);
}

pub fn funding_index_from_capnp(
    r: rpc_capnp::funding_index::Reader<'_>,
) -> capnp::Result<FundingIndex> {
    Ok(FundingIndex {
        market: deriv_id_from_capnp(r.get_market()?)?,
        cumulative: signed_from_capnp(r.get_cumulative()?)?,
        last_tick_height: r.get_last_tick_height(),
        rate_q9: r.get_rate_q9(),
    })
}

// ---- OraclePrice ----

pub fn oracle_price_to_capnp(mut b: rpc_capnp::oracle_price::Builder<'_>, o: &OraclePrice) {
    deriv_id_to_capnp(b.reborrow().init_market(), &o.market);
    u128_to_capnp(b.reborrow().init_px_q8(), o.px_q8);
    b.set_source_height(o.source_height);
    address_to_capnp(b.init_authority(), &o.authority);
}

pub fn oracle_price_from_capnp(
    r: rpc_capnp::oracle_price::Reader<'_>,
) -> capnp::Result<OraclePrice> {
    Ok(OraclePrice {
        market: deriv_id_from_capnp(r.get_market()?)?,
        px_q8: u128_from_capnp(r.get_px_q8()?),
        source_height: r.get_source_height(),
        authority: address_from_capnp(r.get_authority()?)?,
    })
}

// ---- MarkPrice ----

pub fn mark_price_to_capnp(mut b: rpc_capnp::mark_price::Builder<'_>, m: &MarkPrice) {
    deriv_id_to_capnp(b.reborrow().init_market(), &m.market);
    u128_to_capnp(b.reborrow().init_px_q8(), m.px_q8);
    b.set_computed_at_height(m.computed_at_height);
    b.set_premium_ema_q9(m.premium_ema_q9);
}

pub fn mark_price_from_capnp(r: rpc_capnp::mark_price::Reader<'_>) -> capnp::Result<MarkPrice> {
    Ok(MarkPrice {
        market: deriv_id_from_capnp(r.get_market()?)?,
        px_q8: u128_from_capnp(r.get_px_q8()?),
        computed_at_height: r.get_computed_at_height(),
        premium_ema_q9: r.get_premium_ema_q9(),
    })
}

// ---- PerpMarket ----
//
// `paused_reduce_only_only` is M4's reduce-only halt flag. Until M4 lands on
// `cb-types::PerpMarket` we encode `false` and ignore the wire field on
// decode. The schema reserves the field number; M4 swaps `false` for the real
// struct field without reordering. (Decode tolerates either side.)

pub fn perp_market_to_capnp(mut b: rpc_capnp::perp_market::Builder<'_>, m: &PerpMarket) {
    deriv_id_to_capnp(b.reborrow().init_id(), &m.id);
    b.set_underlying(asset_to_capnp(m.underlying));
    u128_to_capnp(b.reborrow().init_tick_size(), m.tick_size);
    u128_to_capnp(b.reborrow().init_lot_size(), m.lot_size);
    b.set_max_leverage_x100(m.max_leverage_x100);
    b.set_initial_margin_bps(m.initial_margin_bps);
    b.set_maintenance_margin_bps(m.maintenance_margin_bps);
    b.set_taker_fee_bps(m.taker_fee_bps);
    b.set_maker_fee_bps_signed(m.maker_fee_bps_signed);
    b.set_funding_interval_blocks(m.funding_interval_blocks);
    b.set_funding_rate_cap_q9(m.funding_rate_cap_q9);
    address_to_capnp(b.reborrow().init_oracle_authority(), &m.oracle_authority);
    u128_to_capnp(
        b.reborrow().init_max_position_notional(),
        m.max_position_notional,
    );
    b.set_paused(m.paused);
    u128_to_capnp(b.reborrow().init_last_trade_px_q8(), m.last_trade_px_q8);
    b.set_last_trade_height(m.last_trade_height);
    // M4: encode the halt flag.
    b.set_paused_reduce_only_only(m.paused_reduce_only_only);
}

pub fn perp_market_from_capnp(r: rpc_capnp::perp_market::Reader<'_>) -> capnp::Result<PerpMarket> {
    Ok(PerpMarket {
        paused_reduce_only_only: r.get_paused_reduce_only_only(),
        id: deriv_id_from_capnp(r.get_id()?)?,
        underlying: asset_from_capnp(r.get_underlying()?),
        tick_size: u128_from_capnp(r.get_tick_size()?),
        lot_size: u128_from_capnp(r.get_lot_size()?),
        max_leverage_x100: r.get_max_leverage_x100(),
        initial_margin_bps: r.get_initial_margin_bps(),
        maintenance_margin_bps: r.get_maintenance_margin_bps(),
        taker_fee_bps: r.get_taker_fee_bps(),
        maker_fee_bps_signed: r.get_maker_fee_bps_signed(),
        funding_interval_blocks: r.get_funding_interval_blocks(),
        funding_rate_cap_q9: r.get_funding_rate_cap_q9(),
        oracle_authority: address_from_capnp(r.get_oracle_authority()?)?,
        max_position_notional: u128_from_capnp(r.get_max_position_notional()?),
        paused: r.get_paused(),
        last_trade_px_q8: u128_from_capnp(r.get_last_trade_px_q8()?),
        last_trade_height: r.get_last_trade_height(),
    })
}

/// Read just the M4 halt flag off the wire. Surfaced separately from
/// `perp_market_from_capnp` because the Rust struct doesn't carry the field
/// pre-M4 and we don't want to bend the type's shape from the RPC layer.
pub fn perp_market_paused_reduce_only_only_from_capnp(
    r: rpc_capnp::perp_market::Reader<'_>,
) -> bool {
    r.get_paused_reduce_only_only()
}

// ---- RejectedEntry ----

pub fn rejected_entry_to_capnp(
    mut b: rpc_capnp::rejected_tx::Builder<'_>,
    e: &crate::stream::RejectedEntry,
) {
    b.set_seq(e.seq);
    b.set_height(e.height);
    h256_to_capnp(b.reborrow().init_hash(), &e.hash);
    address_to_capnp(b.reborrow().init_sender(), &e.sender);
    b.set_nonce(e.nonce);
    b.set_reason(e.reason.as_str());
}

pub fn rejected_entry_from_capnp(
    r: rpc_capnp::rejected_tx::Reader<'_>,
) -> capnp::Result<crate::stream::RejectedEntry> {
    Ok(crate::stream::RejectedEntry {
        seq: r.get_seq(),
        height: r.get_height(),
        hash: h256_from_capnp(r.get_hash()?)?,
        sender: address_from_capnp(r.get_sender()?)?,
        nonce: r.get_nonce(),
        reason: capped_text(r.get_reason()?, "reason", MAX_EXTERNAL_REF_LEN)?,
    })
}

// ---- SpotMarket ----

pub fn spot_market_to_capnp(mut b: rpc_capnp::spot_market::Builder<'_>, m: &MarketConfig) {
    b.set_id(market_to_capnp(m.id));
    b.set_base(asset_to_capnp(m.base));
    b.set_quote(asset_to_capnp(m.quote));
    u128_to_capnp(b.reborrow().init_tick_size(), m.tick_size);
    u128_to_capnp(b.reborrow().init_min_qty(), m.min_qty);
    b.set_fee_bps(m.fee_bps);
}

pub fn spot_market_from_capnp(
    r: rpc_capnp::spot_market::Reader<'_>,
) -> capnp::Result<MarketConfig> {
    Ok(MarketConfig {
        id: market_from_capnp(r.get_id()?),
        base: asset_from_capnp(r.get_base()?),
        quote: asset_from_capnp(r.get_quote()?),
        tick_size: u128_from_capnp(r.get_tick_size()?),
        min_qty: u128_from_capnp(r.get_min_qty()?),
        fee_bps: r.get_fee_bps(),
    })
}

// ---- FutureMarket ----

pub fn future_market_to_capnp(mut b: rpc_capnp::future_market::Builder<'_>, m: &FutureMarket) {
    deriv_id_to_capnp(b.reborrow().init_id(), &m.id);
    b.set_underlying(asset_to_capnp(m.underlying));
    u128_to_capnp(b.reborrow().init_tick_size(), m.tick_size);
    u128_to_capnp(b.reborrow().init_lot_size(), m.lot_size);
    b.set_max_leverage_x100(m.max_leverage_x100);
    b.set_initial_margin_bps(m.initial_margin_bps);
    b.set_maintenance_margin_bps(m.maintenance_margin_bps);
    b.set_taker_fee_bps(m.taker_fee_bps);
    b.set_maker_fee_bps_signed(m.maker_fee_bps_signed);
    b.set_expiry_height(m.expiry_height);
    b.set_settlement_method(m.settlement_method);
    b.set_settlement_window_blocks(m.settlement_window_blocks);
    address_to_capnp(b.reborrow().init_oracle_authority(), &m.oracle_authority);
    b.set_paused(m.paused);
    match m.final_settlement_px_q8 {
        Some(px) => {
            b.set_has_final_settlement_px(true);
            u128_to_capnp(b.reborrow().init_final_settlement_px_q8(), px);
        }
        None => {
            b.set_has_final_settlement_px(false);
            u128_to_capnp(b.reborrow().init_final_settlement_px_q8(), 0);
        }
    }
    // M5: futures last-trade plumbing.
    u128_to_capnp(b.reborrow().init_last_trade_px_q8(), m.last_trade_px_q8);
    b.set_last_trade_height(m.last_trade_height);
    b.set_paused_reduce_only_only(m.paused_reduce_only_only);
}

pub fn future_market_from_capnp(
    r: rpc_capnp::future_market::Reader<'_>,
) -> capnp::Result<FutureMarket> {
    let final_px = if r.get_has_final_settlement_px() {
        Some(u128_from_capnp(r.get_final_settlement_px_q8()?))
    } else {
        None
    };
    Ok(FutureMarket {
        id: deriv_id_from_capnp(r.get_id()?)?,
        underlying: asset_from_capnp(r.get_underlying()?),
        tick_size: u128_from_capnp(r.get_tick_size()?),
        lot_size: u128_from_capnp(r.get_lot_size()?),
        max_leverage_x100: r.get_max_leverage_x100(),
        initial_margin_bps: r.get_initial_margin_bps(),
        maintenance_margin_bps: r.get_maintenance_margin_bps(),
        taker_fee_bps: r.get_taker_fee_bps(),
        maker_fee_bps_signed: r.get_maker_fee_bps_signed(),
        expiry_height: r.get_expiry_height(),
        settlement_method: r.get_settlement_method(),
        settlement_window_blocks: r.get_settlement_window_blocks(),
        oracle_authority: address_from_capnp(r.get_oracle_authority()?)?,
        paused: r.get_paused(),
        final_settlement_px_q8: final_px,
        last_trade_px_q8: u128_from_capnp(r.get_last_trade_px_q8()?),
        last_trade_height: r.get_last_trade_height(),
        paused_reduce_only_only: r.get_paused_reduce_only_only(),
    })
}

// ---- LiquidationEntry ----

pub fn liquidation_entry_to_capnp(
    mut b: rpc_capnp::liquidation_entry::Builder<'_>,
    e: &LiquidationEntry,
) {
    address_to_capnp(b.reborrow().init_owner(), &e.owner);
    deriv_id_to_capnp(b.reborrow().init_market(), &e.market);
    address_to_capnp(b.reborrow().init_triggered_by(), &e.triggered_by);
    b.set_triggered_at_height(e.triggered_at_height);
    u128_to_capnp(b.reborrow().init_mark_at_trigger_q8(), e.mark_at_trigger_q8);
    b.set_seq(e.seq);
}

pub fn liquidation_entry_from_capnp(
    r: rpc_capnp::liquidation_entry::Reader<'_>,
) -> capnp::Result<LiquidationEntry> {
    Ok(LiquidationEntry {
        owner: address_from_capnp(r.get_owner()?)?,
        market: deriv_id_from_capnp(r.get_market()?)?,
        triggered_by: address_from_capnp(r.get_triggered_by()?)?,
        triggered_at_height: r.get_triggered_at_height(),
        mark_at_trigger_q8: u128_from_capnp(r.get_mark_at_trigger_q8()?),
        seq: r.get_seq(),
    })
}

// ---- Perp orderbook snapshot ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerpBookLevel {
    pub price_q8: u128,
    pub qty: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerpOrderBookSnapshot {
    pub market: DerivId,
    pub bids: Vec<PerpBookLevel>,
    pub asks: Vec<PerpBookLevel>,
    pub as_of_height: u64,
}

pub fn perp_book_snapshot_to_capnp(
    mut b: rpc_capnp::perp_order_book_snapshot::Builder<'_>,
    s: &PerpOrderBookSnapshot,
) {
    deriv_id_to_capnp(b.reborrow().init_market(), &s.market);
    {
        let mut bids = b.reborrow().init_bids(s.bids.len() as u32);
        for (i, lvl) in s.bids.iter().enumerate() {
            let mut entry = bids.reborrow().get(i as u32);
            u128_to_capnp(entry.reborrow().init_price_q8(), lvl.price_q8);
            u128_to_capnp(entry.init_qty(), lvl.qty);
        }
    }
    {
        let mut asks = b.reborrow().init_asks(s.asks.len() as u32);
        for (i, lvl) in s.asks.iter().enumerate() {
            let mut entry = asks.reborrow().get(i as u32);
            u128_to_capnp(entry.reborrow().init_price_q8(), lvl.price_q8);
            u128_to_capnp(entry.init_qty(), lvl.qty);
        }
    }
    b.set_as_of_height(s.as_of_height);
}

pub fn perp_book_snapshot_from_capnp(
    r: rpc_capnp::perp_order_book_snapshot::Reader<'_>,
) -> capnp::Result<PerpOrderBookSnapshot> {
    let market = deriv_id_from_capnp(r.get_market()?)?;
    let mut bids = Vec::new();
    let bids_r = r.get_bids()?;
    for i in 0..bids_r.len() {
        let e = bids_r.get(i);
        bids.push(PerpBookLevel {
            price_q8: u128_from_capnp(e.get_price_q8()?),
            qty: u128_from_capnp(e.get_qty()?),
        });
    }
    let mut asks = Vec::new();
    let asks_r = r.get_asks()?;
    for i in 0..asks_r.len() {
        let e = asks_r.get(i);
        asks.push(PerpBookLevel {
            price_q8: u128_from_capnp(e.get_price_q8()?),
            qty: u128_from_capnp(e.get_qty()?),
        });
    }
    Ok(PerpOrderBookSnapshot {
        market,
        bids,
        asks,
        as_of_height: r.get_as_of_height(),
    })
}

// ---- Spot orderbook snapshot ----
//
// Mirrors `PerpOrderBookSnapshot` exactly, swapping `DerivId` for `MarketId`
// and `price_q8` for plain `price` (spot uses `MarketConfig.tick_size` units
// directly — no q8 scaling on the wire). The schema's `OrderBookSnapshot` and
// `OrderBookLevel` already exist as the spot shape; the only divergence from
// perp is the field name `price` vs `priceQ8`.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpotBookLevel {
    pub price: u128,
    pub qty: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpotOrderBookSnapshot {
    pub market: MarketId,
    pub bids: Vec<SpotBookLevel>,
    pub asks: Vec<SpotBookLevel>,
    pub as_of_height: u64,
}

pub fn spot_book_snapshot_to_capnp(
    mut b: rpc_capnp::order_book_snapshot::Builder<'_>,
    s: &SpotOrderBookSnapshot,
) {
    b.set_market(market_to_capnp(s.market));
    {
        let mut bids = b.reborrow().init_bids(s.bids.len() as u32);
        for (i, lvl) in s.bids.iter().enumerate() {
            let mut entry = bids.reborrow().get(i as u32);
            u128_to_capnp(entry.reborrow().init_price(), lvl.price);
            u128_to_capnp(entry.init_qty(), lvl.qty);
        }
    }
    {
        let mut asks = b.reborrow().init_asks(s.asks.len() as u32);
        for (i, lvl) in s.asks.iter().enumerate() {
            let mut entry = asks.reborrow().get(i as u32);
            u128_to_capnp(entry.reborrow().init_price(), lvl.price);
            u128_to_capnp(entry.init_qty(), lvl.qty);
        }
    }
    b.set_as_of_height(s.as_of_height);
}

pub fn spot_book_snapshot_from_capnp(
    r: rpc_capnp::order_book_snapshot::Reader<'_>,
) -> capnp::Result<SpotOrderBookSnapshot> {
    let market = market_from_capnp(r.get_market()?);
    let mut bids = Vec::new();
    let bids_r = r.get_bids()?;
    for i in 0..bids_r.len() {
        let e = bids_r.get(i);
        bids.push(SpotBookLevel {
            price: u128_from_capnp(e.get_price()?),
            qty: u128_from_capnp(e.get_qty()?),
        });
    }
    let mut asks = Vec::new();
    let asks_r = r.get_asks()?;
    for i in 0..asks_r.len() {
        let e = asks_r.get(i);
        asks.push(SpotBookLevel {
            price: u128_from_capnp(e.get_price()?),
            qty: u128_from_capnp(e.get_qty()?),
        });
    }
    Ok(SpotOrderBookSnapshot {
        market,
        bids,
        asks,
        as_of_height: r.get_as_of_height(),
    })
}

// ============================================================================
// Round-trip tests for the M6 wire surfaces.
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::{AssetId, DerivId, MarketId, Side};

    // -----------------------------------------------------------------------
    // Ordinal agreement between the Rust enums and the Cap'n Proto schema.
    //
    // `AssetId::as_u8()` is a map key in `AccountState` and the `bincode`
    // variant index every transaction signature commits to; the Cap'n Proto
    // enumerant is what goes on the wire. Nothing forces the two numberings to
    // agree — the mapping functions above are hand-written matches — so if they
    // ever diverge, a transfer signed for one asset arrives as another and the
    // only symptom is a balance in the wrong column.
    //
    // These two tests are the tripwire. They are here rather than in `cb-types`
    // because this is the only crate that can see both enums.
    // -----------------------------------------------------------------------

    #[test]
    fn asset_ordinals_match_capnp() {
        for a in AssetId::ALL {
            let wire: u16 = asset_to_capnp(a).into();
            assert_eq!(
                wire,
                a.as_u8() as u16,
                "{a:?}: Rust ordinal {} but capnp enumerant {wire}",
                a.as_u8()
            );
            // ...and the mapping is a bijection, so no two assets collapse.
            assert_eq!(asset_from_capnp(asset_to_capnp(a)), a);
        }
        // Every capnp enumerant maps back, so the schema has not grown a value
        // this build cannot name.
        for ord in 0..AssetId::ALL.len() as u16 {
            let wire = types_capnp::AssetId::try_from(ord)
                .unwrap_or_else(|e| panic!("capnp AssetId ordinal {ord} missing: {e:?}"));
            assert_eq!(asset_from_capnp(wire).as_u8() as u16, ord);
        }
        assert!(
            types_capnp::AssetId::try_from(AssetId::ALL.len() as u16).is_err(),
            "capnp AssetId has more enumerants than the Rust enum"
        );
    }

    #[test]
    fn market_ordinals_match_capnp() {
        for m in MarketId::ALL {
            let wire: u16 = market_to_capnp(m).into();
            assert_eq!(
                wire,
                m.as_u8() as u16,
                "{m:?}: Rust ordinal {} but capnp enumerant {wire}",
                m.as_u8()
            );
            assert_eq!(market_from_capnp(market_to_capnp(m)), m);
        }
        for ord in 0..MarketId::ALL.len() as u16 {
            let wire = types_capnp::MarketId::try_from(ord)
                .unwrap_or_else(|e| panic!("capnp MarketId ordinal {ord} missing: {e:?}"));
            assert_eq!(market_from_capnp(wire).as_u8() as u16, ord);
        }
        assert!(
            types_capnp::MarketId::try_from(MarketId::ALL.len() as u16).is_err(),
            "capnp MarketId has more enumerants than the Rust enum"
        );
    }

    #[test]
    fn legacy_wire_ordinals_are_frozen() {
        // Spelled out as literals: these three values are written into every
        // historical signature and every persisted balance row.
        assert_eq!(u16::from(asset_to_capnp(AssetId::Btc)), 0);
        assert_eq!(u16::from(asset_to_capnp(AssetId::Eth)), 1);
        assert_eq!(u16::from(asset_to_capnp(AssetId::Usd)), 2);
        assert_eq!(u16::from(market_to_capnp(MarketId::BtcUsd)), 0);
        assert_eq!(u16::from(market_to_capnp(MarketId::EthUsd)), 1);
    }

    #[test]
    fn an_unknown_balance_ordinal_is_an_error_not_a_usd_balance() {
        // `asset_byte_to_capnp` used to fall back to USD for any unrecognised
        // byte. With a widened universe that fallback would report an unknown
        // asset's balance as dollars. Assert it errors instead.
        assert!(asset_byte_to_capnp(AssetId::Spy.as_u8()).is_ok());
        let err = asset_byte_to_capnp(200).expect_err("unknown ordinal must fail");
        assert!(
            err.to_string().contains("200"),
            "error should name the bad ordinal, got `{err}`"
        );
    }

    fn build_message<F>(f: F) -> capnp::message::Builder<capnp::message::HeapAllocator>
    where
        F: FnOnce(&mut capnp::message::Builder<capnp::message::HeapAllocator>),
    {
        let mut msg = capnp::message::Builder::new_default();
        f(&mut msg);
        msg
    }

    #[test]
    fn signed_round_trip() {
        for v in [-1_234_567i128, 0, 1, i128::MIN + 1, i128::MAX] {
            let s = Signed::from_i128(v);
            let mut msg = capnp::message::Builder::new_default();
            let root = msg.init_root::<types_capnp::signed_i128::Builder<'_>>();
            signed_to_capnp(root, &s);
            let reader = msg
                .get_root_as_reader::<types_capnp::signed_i128::Reader<'_>>()
                .unwrap();
            assert_eq!(signed_from_capnp(reader).unwrap(), s);
        }
    }

    #[test]
    fn position_round_trip() {
        let p = Position {
            owner: Address([1; 20]),
            market: DerivId::perp(7),
            side: Side::Sell,
            qty: 999,
            entry_funding_index: Signed::from_i128(-42),
            entry_px_q8: 5_000 * 100_000_000,
            margin_mode: 1,
            isolated_margin: 1_000_000,
            last_funding_height: 1234,
        };
        let mut msg = capnp::message::Builder::new_default();
        let root = msg.init_root::<rpc_capnp::position::Builder<'_>>();
        position_to_capnp(root, &p);
        let reader = msg
            .get_root_as_reader::<rpc_capnp::position::Reader<'_>>()
            .unwrap();
        assert_eq!(position_from_capnp(reader).unwrap(), p);
    }

    #[test]
    fn margin_account_round_trip() {
        let m = MarginAccount {
            owner: Address([2; 20]),
            collateral: 500_000_000,
            unrealized_pnl: Signed::from_i128(-7_500),
            bad_debt: 0,
        };
        let mut msg = capnp::message::Builder::new_default();
        let root = msg.init_root::<rpc_capnp::margin_account::Builder<'_>>();
        margin_account_to_capnp(root, &m);
        let reader = msg
            .get_root_as_reader::<rpc_capnp::margin_account::Reader<'_>>()
            .unwrap();
        assert_eq!(margin_account_from_capnp(reader).unwrap(), m);
    }

    #[test]
    fn funding_index_round_trip() {
        let f = FundingIndex {
            market: DerivId::perp(0),
            cumulative: Signed::from_i128(98_765),
            last_tick_height: 7_200,
            rate_q9: -250_000,
        };
        let mut msg = capnp::message::Builder::new_default();
        let root = msg.init_root::<rpc_capnp::funding_index::Builder<'_>>();
        funding_index_to_capnp(root, &f);
        let reader = msg
            .get_root_as_reader::<rpc_capnp::funding_index::Reader<'_>>()
            .unwrap();
        assert_eq!(funding_index_from_capnp(reader).unwrap(), f);
    }

    #[test]
    fn oracle_and_mark_round_trip() {
        let o = OraclePrice {
            market: DerivId::perp(0),
            px_q8: 50_000 * 100_000_000,
            source_height: 1234,
            authority: Address([7; 20]),
        };
        let mut msg = capnp::message::Builder::new_default();
        let root = msg.init_root::<rpc_capnp::oracle_price::Builder<'_>>();
        oracle_price_to_capnp(root, &o);
        let reader = msg
            .get_root_as_reader::<rpc_capnp::oracle_price::Reader<'_>>()
            .unwrap();
        assert_eq!(oracle_price_from_capnp(reader).unwrap(), o);

        let m = MarkPrice {
            market: DerivId::perp(0),
            px_q8: 50_001 * 100_000_000,
            computed_at_height: 1234,
            premium_ema_q9: 12_345,
        };
        let mut msg = capnp::message::Builder::new_default();
        let root = msg.init_root::<rpc_capnp::mark_price::Builder<'_>>();
        mark_price_to_capnp(root, &m);
        let reader = msg
            .get_root_as_reader::<rpc_capnp::mark_price::Reader<'_>>()
            .unwrap();
        assert_eq!(mark_price_from_capnp(reader).unwrap(), m);
    }

    fn sample_perp_market() -> PerpMarket {
        PerpMarket {
            id: DerivId::perp(3),
            underlying: AssetId::Btc,
            tick_size: 100,
            lot_size: 1_000,
            max_leverage_x100: 5_000,
            initial_margin_bps: 200,
            maintenance_margin_bps: 100,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            funding_interval_blocks: 3_600,
            funding_rate_cap_q9: 1_000_000,
            oracle_authority: Address([4; 20]),
            max_position_notional: 100_000_000_000_000,
            paused: false,
            last_trade_px_q8: 51_234 * 100_000_000,
            last_trade_height: 5,
            paused_reduce_only_only: false,
        }
    }

    #[test]
    fn perp_market_round_trip() {
        let p = sample_perp_market();
        let mut msg = capnp::message::Builder::new_default();
        let root = msg.init_root::<rpc_capnp::perp_market::Builder<'_>>();
        perp_market_to_capnp(root, &p);
        let reader = msg
            .get_root_as_reader::<rpc_capnp::perp_market::Reader<'_>>()
            .unwrap();
        assert_eq!(perp_market_from_capnp(reader).unwrap(), p);
        // M4 reduce-only halt flag default-encodes to false.
        assert!(!perp_market_paused_reduce_only_only_from_capnp(reader));
    }

    #[test]
    fn future_market_round_trip_with_and_without_settlement_px() {
        let f = FutureMarket {
            id: DerivId::future(2),
            underlying: AssetId::Eth,
            tick_size: 100,
            lot_size: 1_000,
            max_leverage_x100: 2_500,
            initial_margin_bps: 400,
            maintenance_margin_bps: 200,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            expiry_height: 2_592_000,
            settlement_method: 0,
            settlement_window_blocks: 600,
            oracle_authority: Address([4; 20]),
            paused: false,
            final_settlement_px_q8: None,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        };
        for variant in [
            f.clone(),
            FutureMarket {
                final_settlement_px_q8: Some(3_200 * 100_000_000),
                last_trade_px_q8: 3_180 * 100_000_000,
                last_trade_height: 4,
                paused_reduce_only_only: false,
                ..f.clone()
            },
        ] {
            let mut msg = capnp::message::Builder::new_default();
            let root = msg.init_root::<rpc_capnp::future_market::Builder<'_>>();
            future_market_to_capnp(root, &variant);
            let reader = msg
                .get_root_as_reader::<rpc_capnp::future_market::Reader<'_>>()
                .unwrap();
            assert_eq!(future_market_from_capnp(reader).unwrap(), variant);
        }
    }

    #[test]
    fn liquidation_entry_round_trip() {
        let e = LiquidationEntry {
            owner: Address([5; 20]),
            market: DerivId::perp(1),
            triggered_by: Address([6; 20]),
            triggered_at_height: 999,
            mark_at_trigger_q8: 45_000 * 100_000_000,
            seq: 7,
        };
        let mut msg = capnp::message::Builder::new_default();
        let root = msg.init_root::<rpc_capnp::liquidation_entry::Builder<'_>>();
        liquidation_entry_to_capnp(root, &e);
        let reader = msg
            .get_root_as_reader::<rpc_capnp::liquidation_entry::Reader<'_>>()
            .unwrap();
        assert_eq!(liquidation_entry_from_capnp(reader).unwrap(), e);
    }

    #[test]
    fn perp_book_snapshot_round_trip() {
        let s = PerpOrderBookSnapshot {
            market: DerivId::perp(0),
            bids: vec![
                PerpBookLevel {
                    price_q8: 100,
                    qty: 10,
                },
                PerpBookLevel {
                    price_q8: 99,
                    qty: 20,
                },
            ],
            asks: vec![
                PerpBookLevel {
                    price_q8: 101,
                    qty: 5,
                },
                PerpBookLevel {
                    price_q8: 102,
                    qty: 7,
                },
            ],
            as_of_height: 1_000,
        };
        let mut msg = capnp::message::Builder::new_default();
        let root = msg.init_root::<rpc_capnp::perp_order_book_snapshot::Builder<'_>>();
        perp_book_snapshot_to_capnp(root, &s);
        let reader = msg
            .get_root_as_reader::<rpc_capnp::perp_order_book_snapshot::Reader<'_>>()
            .unwrap();
        assert_eq!(perp_book_snapshot_from_capnp(reader).unwrap(), s);
    }

    #[test]
    fn spot_book_snapshot_round_trip() {
        // Mirrors `perp_book_snapshot_round_trip` against the spot wire shape.
        // Empty-side and multi-level cases both round-trip byte-identical.
        for s in [
            SpotOrderBookSnapshot {
                market: MarketId::BtcUsd,
                bids: vec![],
                asks: vec![],
                as_of_height: 0,
            },
            SpotOrderBookSnapshot {
                market: MarketId::EthUsd,
                bids: vec![
                    SpotBookLevel {
                        price: 100,
                        qty: 15,
                    },
                    SpotBookLevel { price: 99, qty: 7 },
                ],
                asks: vec![
                    SpotBookLevel {
                        price: 101,
                        qty: 20,
                    },
                    SpotBookLevel {
                        price: 102,
                        qty: 30,
                    },
                ],
                as_of_height: 1_234,
            },
        ] {
            let mut msg = capnp::message::Builder::new_default();
            let root = msg.init_root::<rpc_capnp::order_book_snapshot::Builder<'_>>();
            spot_book_snapshot_to_capnp(root, &s);
            let reader = msg
                .get_root_as_reader::<rpc_capnp::order_book_snapshot::Reader<'_>>()
                .unwrap();
            assert_eq!(spot_book_snapshot_from_capnp(reader).unwrap(), s);
        }
    }

    // Suppress dead-code lint for unused private helper above.
    #[allow(dead_code)]
    fn _build_message_unused_smoke() {
        let _ = build_message(|_| ());
    }
}
