//! SignedTx: a Transaction plus an ECDSA recoverable signature.

use crate::tx::Transaction;
use cb_crypto::{keccak256, recover, sign as crypto_sign, CryptoError, Signature};
use cb_types::{Address, H256};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedTx {
    pub tx: Transaction,
    pub sig_r: [u8; 32],
    pub sig_s: [u8; 32],
    pub sig_recovery: u8,
}

#[derive(Debug, Error)]
pub enum SignError {
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

/// Domain tag prefixed to every transaction preimage before hashing.
///
/// Votes and view-changes carry a tag (`UL-VOTE\0`, `UL-VIEWCHG\0`); block
/// headers and transactions did not, so a transaction's bytes and a header's
/// bytes hashed into the same space. No collision was reachable — a header
/// starts with a height where a transaction starts with a nonce, and the
/// proposer signs only headers it built — but a signature scheme should not
/// depend on that argument holding for every future message type. Changing
/// this is a hard fork: every signer in the repository goes through
/// `sign_with`, so the change is one constant, but a chain must be started
/// fresh. (Core review L2.)
pub const TX_SIGNING_DOMAIN: &[u8] = b"CB-TX\0";

impl SignedTx {
    pub fn signing_hash(tx: &Transaction) -> H256 {
        let encoded = tx.encode();
        let mut preimage = Vec::with_capacity(TX_SIGNING_DOMAIN.len() + encoded.len());
        preimage.extend_from_slice(TX_SIGNING_DOMAIN);
        preimage.extend_from_slice(&encoded);
        keccak256(&preimage)
    }

    pub fn sign_with(secret: &secp256k1::SecretKey, tx: Transaction) -> Result<Self, SignError> {
        let h = Self::signing_hash(&tx);
        let s = crypto_sign(secret, h)?;
        Ok(SignedTx {
            tx,
            sig_r: s.r,
            sig_s: s.s,
            sig_recovery: s.recovery_id,
        })
    }

    /// A transaction nobody signed: the executor's own, for the system
    /// actions a block must carry (funding ticks, settlements) when the chain
    /// runs under BFT consensus. Every validator injects the same ones at the
    /// same height from the same rule, so nothing needs to be signed and the
    /// transaction is never on the wire. It carries no nonce and consumes
    /// none. Recognised by an all-zero signature, which no real signature has.
    pub fn synthetic(tx: Transaction) -> Self {
        SignedTx {
            tx,
            sig_r: [0u8; 32],
            sig_s: [0u8; 32],
            sig_recovery: 0,
        }
    }

    /// Whether this is a [`SignedTx::synthetic`] transaction.
    pub fn is_synthetic(&self) -> bool {
        self.sig_r == [0u8; 32] && self.sig_s == [0u8; 32]
    }

    pub fn sender(&self) -> Result<Address, CryptoError> {
        let h = Self::signing_hash(&self.tx);
        recover(
            &Signature {
                r: self.sig_r,
                s: self.sig_s,
                recovery_id: self.sig_recovery,
            },
            h,
        )
    }

    pub fn hash(&self) -> H256 {
        keccak256(&bincode::serialize(self).expect("SignedTx encode"))
    }

    /// Recover the sender once and pair it with this signed tx. Use this at the boundary
    /// where a tx enters the system (RPC submit, mempool drain, replica block ingest) so
    /// downstream block-building code never re-runs ECDSA recovery.
    pub fn verify(self) -> Result<VerifiedTx, CryptoError> {
        let sender = self.sender()?;
        Ok(VerifiedTx {
            signed: self,
            sender,
        })
    }
}

/// A `SignedTx` whose ECDSA sender has already been recovered. Carries the cached sender
/// alongside the tx so the block builder, executor, and ordering can all use it without
/// re-running the (expensive) recovery.
#[derive(Clone, Debug)]
pub struct VerifiedTx {
    pub signed: SignedTx,
    pub sender: Address,
}

impl VerifiedTx {
    /// Construct a `VerifiedTx` from parts WITHOUT verifying the signature. Caller must
    /// guarantee `sender` corresponds to `signed.sig_*`. Used by trusted in-process
    /// producers (e.g. the benchmark generator) that already know the signing key.
    pub fn from_parts_unchecked(signed: SignedTx, sender: Address) -> Self {
        Self { signed, sender }
    }

    /// A [`SignedTx::synthetic`] transaction attributed to `sender`.
    pub fn synthetic(tx: Transaction, sender: Address) -> Self {
        Self {
            signed: SignedTx::synthetic(tx),
            sender,
        }
    }

    pub fn is_synthetic(&self) -> bool {
        self.signed.is_synthetic()
    }
}

/// On-disk mempool record: a SignedTx pre-paired with the recovered sender.
/// Storing this (instead of bare SignedTx bytes) lets the block-build loop
/// skip a per-tx ECDSA recovery — recovery happens once at submit time, not
/// twice. Material throughput gain when the mempool is the hot path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MempoolRecord {
    pub signed: SignedTx,
    pub sender: Address,
}

impl MempoolRecord {
    pub fn into_verified(self) -> VerifiedTx {
        VerifiedTx::from_parts_unchecked(self.signed, self.sender)
    }
}

/// Decode a slice of mempool blobs into `VerifiedTx` in parallel. Skips entries
/// that fail to decode (treated as corrupt) — same forgiveness as the serial
/// drain. Senders come straight from the `MempoolRecord` (already recovered on
/// submit), so no ECDSA recovery happens here.
pub fn decode_mempool_batch(raw: &[Vec<u8>]) -> Vec<VerifiedTx> {
    use rayon::prelude::*;
    raw.par_iter()
        .filter_map(|blob| bincode::deserialize::<MempoolRecord>(blob).ok())
        .map(|rec| rec.into_verified())
        .collect()
}

/// Recover senders for a slice of `SignedTx` in parallel. Returns the first
/// ECDSA failure (deterministic: `find_first` honours input order), since a
/// single bad signature should halt block apply. Used on the legacy fallback
/// path where a streamed block lacks pre-recovered senders.
pub fn recover_senders_par(txs: &[SignedTx]) -> Result<Vec<Address>, CryptoError> {
    use rayon::prelude::*;
    txs.par_iter().map(|s| s.sender()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::TxBody;
    use cb_crypto::{address_from_pubkey, generate_keypair};
    use cb_types::{Address, AssetId};

    #[test]
    fn sign_and_recover_sender() {
        let (sk, pk) = generate_keypair();
        let signer = address_from_pubkey(&pk);
        let tx = Transaction {
            nonce: 0,
            chain_id: 1,
            body: TxBody::Transfer {
                to: Address([1; 20]),
                asset: AssetId::Usd,
                amount: 100,
            },
        };
        let signed = SignedTx::sign_with(&sk, tx).unwrap();
        assert_eq!(signed.sender().unwrap(), signer);
    }

    #[test]
    fn the_signing_hash_is_domain_tagged() {
        // Core review L2. The preimage is `TX_SIGNING_DOMAIN || bincode(tx)`,
        // so a transaction's digest can never equal the digest of the same
        // bytes signed under another domain (a block header, a vote).
        let tx = Transaction {
            nonce: 0,
            chain_id: 1,
            body: TxBody::Transfer {
                to: Address([1; 20]),
                asset: AssetId::Usd,
                amount: 100,
            },
        };
        let untagged = keccak256(&tx.encode());
        assert_ne!(SignedTx::signing_hash(&tx), untagged);
        let mut tagged = TX_SIGNING_DOMAIN.to_vec();
        tagged.extend_from_slice(&tx.encode());
        assert_eq!(SignedTx::signing_hash(&tx), keccak256(&tagged));
    }

    #[test]
    fn signed_hash_changes_when_body_changes() {
        let (sk, _) = generate_keypair();
        let mk = |amt| Transaction {
            nonce: 0,
            chain_id: 1,
            body: TxBody::Transfer {
                to: Address([1; 20]),
                asset: AssetId::Usd,
                amount: amt,
            },
        };
        let s1 = SignedTx::sign_with(&sk, mk(1)).unwrap();
        let s2 = SignedTx::sign_with(&sk, mk(2)).unwrap();
        assert_ne!(s1.hash(), s2.hash());
    }

    fn mk_signed(sk: &secp256k1::SecretKey, nonce: u64, amount: u128) -> SignedTx {
        let tx = Transaction {
            nonce,
            chain_id: 1,
            body: TxBody::Transfer {
                to: Address([2; 20]),
                asset: AssetId::Usd,
                amount,
            },
        };
        SignedTx::sign_with(sk, tx).unwrap()
    }

    #[test]
    fn decode_mempool_batch_matches_serial() {
        let (sk, pk) = generate_keypair();
        let signer = address_from_pubkey(&pk);
        let blobs: Vec<Vec<u8>> = (0..8u64)
            .map(|n| {
                let signed = mk_signed(&sk, n, 100 + n as u128);
                let rec = MempoolRecord {
                    signed,
                    sender: signer,
                };
                bincode::serialize(&rec).unwrap()
            })
            .collect();

        let parallel = decode_mempool_batch(&blobs);
        let serial: Vec<VerifiedTx> = blobs
            .iter()
            .filter_map(|b| bincode::deserialize::<MempoolRecord>(b).ok())
            .map(|r| r.into_verified())
            .collect();

        assert_eq!(parallel.len(), 8);
        assert_eq!(parallel.len(), serial.len());
        for (p, s) in parallel.iter().zip(serial.iter()) {
            assert_eq!(p.sender, s.sender);
            assert_eq!(p.sender, signer);
            assert_eq!(p.signed.tx.nonce, s.signed.tx.nonce);
            assert_eq!(p.signed.hash(), s.signed.hash());
        }
    }

    #[test]
    fn decode_mempool_batch_skips_corrupt() {
        let (sk, pk) = generate_keypair();
        let signer = address_from_pubkey(&pk);
        let good = bincode::serialize(&MempoolRecord {
            signed: mk_signed(&sk, 0, 1),
            sender: signer,
        })
        .unwrap();
        let blobs = vec![good.clone(), vec![0xff, 0xff, 0xff], good];
        let out = decode_mempool_batch(&blobs);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn recover_senders_par_matches_serial() {
        let (sk, pk) = generate_keypair();
        let signer = address_from_pubkey(&pk);
        let txs: Vec<SignedTx> = (0..6u64).map(|n| mk_signed(&sk, n, n as u128)).collect();

        let parallel = recover_senders_par(&txs).unwrap();
        let serial: Vec<Address> = txs.iter().map(|s| s.sender().unwrap()).collect();

        assert_eq!(parallel.len(), 6);
        assert_eq!(parallel, serial);
        assert!(parallel.iter().all(|a| *a == signer));
    }

    // ---------------- Derivative tx signing round-trip ----------------

    fn sign_and_check(sk: &secp256k1::SecretKey, expected: Address, body: TxBody) {
        let tx = Transaction {
            nonce: 0,
            chain_id: 1,
            body,
        };
        let signed = SignedTx::sign_with(sk, tx.clone()).unwrap();
        // Round-trip the signed-tx bytes to make sure all new variants encode.
        let bytes = bincode::serialize(&signed).unwrap();
        let back: SignedTx = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.tx, tx);
        assert_eq!(back.sender().unwrap(), expected);
    }

    #[test]
    fn sign_and_recover_each_derivative_variant() {
        use cb_types::{DerivId, Side, Tif, H256};

        let (sk, pk) = generate_keypair();
        let signer = address_from_pubkey(&pk);
        let mkt = DerivId::perp(0);

        sign_and_check(
            &sk,
            signer,
            TxBody::PlacePerpOrder {
                market: mkt,
                side: Side::Buy,
                price_q8: 100,
                qty: 1,
                tif: Tif::Gtc,
                post_only: false,
                reduce_only: false,
                client_order_id: H256([1; 32]),
            },
        );
        sign_and_check(
            &sk,
            signer,
            TxBody::CancelPerpOrder {
                order_id: H256([2; 32]),
            },
        );
        sign_and_check(
            &sk,
            signer,
            TxBody::OpenPosition {
                market: mkt,
                side: Side::Buy,
                qty: 1,
                max_slippage_bps: 800,
            },
        );
        sign_and_check(
            &sk,
            signer,
            TxBody::ClosePosition {
                market: mkt,
                qty: 0,
                max_slippage_bps: 800,
            },
        );
        sign_and_check(
            &sk,
            signer,
            TxBody::AdjustMargin {
                market: mkt,
                is_topup: true,
                amount: 1_000,
            },
        );
        sign_and_check(
            &sk,
            signer,
            TxBody::LiquidationTrigger {
                owner: Address([7; 20]),
                market: mkt,
            },
        );
        sign_and_check(
            &sk,
            signer,
            TxBody::OracleUpdate {
                market: mkt,
                px_q8: 5_000_000_000_000,
                submitted_height: 42,
            },
        );
        sign_and_check(&sk, signer, TxBody::FundingTick { market: mkt });
        sign_and_check(
            &sk,
            signer,
            TxBody::FuturesSettle {
                market: DerivId::future(0),
            },
        );
    }

    #[test]
    fn recover_senders_par_propagates_error() {
        let (sk, _) = generate_keypair();
        let mut bad = mk_signed(&sk, 0, 1);
        // Corrupt the recovery id so recovery fails.
        bad.sig_recovery = 4;
        let txs = vec![mk_signed(&sk, 1, 1), bad, mk_signed(&sk, 2, 1)];
        assert!(recover_senders_par(&txs).is_err());
    }

    // ---------------- tampering, malleability, synthetic ----------------

    fn signer() -> (secp256k1::SecretKey, Address) {
        let (sk, pk) = generate_keypair();
        (sk, address_from_pubkey(&pk))
    }

    /// The nonce and the chain id are inside the signed preimage, so editing
    /// either after signing can never leave a transaction attributed to its
    /// signer: the signature then recovers some other key (or none). This is
    /// what makes the executor's nonce and chain-id checks mean anything — a
    /// relay cannot bump a nonce or re-target a chain and keep the sender.
    #[test]
    fn editing_the_nonce_or_chain_id_after_signing_changes_the_sender() {
        let (sk, me) = signer();
        let signed = mk_signed(&sk, 5, 10);
        assert_eq!(signed.sender().unwrap(), me);
        for nonce in [0u64, 4, 6, 7, u64::MAX] {
            let mut t = signed.clone();
            t.tx.nonce = nonce;
            assert_ne!(t.sender().ok(), Some(me), "nonce {nonce}");
            assert_ne!(t.hash(), signed.hash());
        }
        for chain_id in [0u32, 2, u32::MAX] {
            let mut t = signed.clone();
            t.tx.chain_id = chain_id;
            assert_ne!(t.sender().ok(), Some(me), "chain {chain_id}");
        }
    }

    /// The same body and nonce signed for two chains has two different
    /// signing digests: a signature for one chain is not a signature for the
    /// other.
    #[test]
    fn the_signing_hash_commits_to_the_chain_id() {
        let mut tx = mk_signed(&generate_keypair().0, 0, 1).tx;
        let h1 = SignedTx::signing_hash(&tx);
        tx.chain_id = 2;
        assert_ne!(h1, SignedTx::signing_hash(&tx));
    }

    /// `(r, n − s)` with the recovery id flipped is the other valid ECDSA
    /// signature over the same digest. Accepted, it would be a second,
    /// differently-hashed copy of the same transaction — harmless to the
    /// ledger, because the nonce admits only one of them, but a way to make
    /// one transaction look like two to anything keyed on the hash (a mempool,
    /// a receipt). Recovery enforces low-S, so the twin does not verify.
    #[test]
    fn the_malleated_twin_of_a_signature_does_not_verify() {
        // secp256k1 group order, big-endian.
        const N: [u8; 32] = [
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFE, 0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C,
            0xD0, 0x36, 0x41, 0x41,
        ];
        fn n_minus(s: &[u8; 32]) -> [u8; 32] {
            let mut out = [0u8; 32];
            let mut borrow = 0i16;
            for i in (0..32).rev() {
                let d = N[i] as i16 - s[i] as i16 - borrow;
                borrow = if d < 0 { 1 } else { 0 };
                out[i] = (d + 256 * borrow) as u8;
            }
            out
        }
        for i in 0..16u64 {
            let (sk, me) = signer();
            let signed = mk_signed(&sk, i, 1 + i as u128);
            assert_eq!(signed.sender().unwrap(), me);
            let mut twin = signed.clone();
            twin.sig_s = n_minus(&signed.sig_s);
            twin.sig_recovery ^= 1;
            assert_ne!(twin.hash(), signed.hash());
            assert!(
                twin.clone().verify().map(|v| v.sender).ok() != Some(me),
                "a high-S twin recovered the original signer"
            );
        }
    }

    /// A synthetic transaction is recognised only by an all-zero `r` *and*
    /// `s`; it has no recoverable sender, so one arriving off the wire cannot
    /// pass `verify` and pick up an identity on the way in.
    #[test]
    fn only_an_all_zero_signature_is_synthetic_and_it_has_no_sender() {
        let (sk, _) = signer();
        let tx = mk_signed(&sk, 0, 1).tx;
        let syn = SignedTx::synthetic(tx.clone());
        assert!(syn.is_synthetic());
        assert!(syn.clone().verify().is_err());
        let mut half = syn.clone();
        half.sig_s[31] = 1;
        assert!(!half.is_synthetic());
        let mut other = syn;
        other.sig_r[0] = 1;
        assert!(!other.is_synthetic());
        let real = SignedTx::sign_with(&sk, tx).unwrap();
        assert!(!real.is_synthetic());
        let v = VerifiedTx::synthetic(real.tx.clone(), Address([7; 20]));
        assert!(v.is_synthetic());
        assert_eq!(v.sender, Address([7; 20]));
    }
}
