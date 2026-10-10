//! Login challenges.
//!
//! A challenge is a nonce the server issues and the client signs. Three
//! properties make it worth having rather than signing a fixed string:
//!
//! * **Single use.** A signature that has been presented once cannot be
//!   presented again, so an observed login cannot be replayed.
//! * **Short lived.** Five minutes, per the contract. A signature captured
//!   from a log or a screen recording stops being useful quickly.
//! * **Server-worded.** The contract tells the client to sign the `message`
//!   verbatim rather than rebuilding it, so the wording can change without
//!   breaking every client. That only works if the server keeps the exact text
//!   it issued and verifies against *that*, which is what this store is for.
//!
//! ## Why several challenges live at once
//!
//! `POST /auth/challenge` needs no session and no proof that the caller owns
//! the address it names — it cannot have one, since proving ownership is what
//! the challenge is *for*. When the store kept exactly one challenge per
//! address, anybody who knew a victim's login address (the leaderboard
//! publishes them) could issue a challenge "for" that victim and evict the one
//! they were part-way through signing, failing their login. So the store keeps
//! a small set of live challenges per address and a signature over **any** of
//! them logs in.
//!
//! The matched challenge, and only it, is removed on a successful login; a
//! signature that matches nothing removes nothing. That is a deliberate change
//! from "consume on every attempt": consuming on failure would simply move the
//! same denial-of-login from `POST /auth/challenge` to `POST /auth/login`. The
//! property it gave up — that a failed attempt cannot hold a nonce open while
//! an attacker guesses — costs nothing here, because the nonce is 128 bits
//! from the OS CSPRNG and is not what a login attempt is guessing.

use cb_types::Address;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use rand::RngCore;

/// Live challenges kept per address.
///
/// Enough that a user with a second tab, or one who clicked twice, is not
/// locked out, and small enough that the set is not itself somewhere to put
/// bytes.
const MAX_PER_ADDRESS: usize = 4;

/// Most addresses with an outstanding challenge.
///
/// Issuing costs the caller one unauthenticated request and there are 2^160
/// addresses to issue for, so the map needs a ceiling as well as the janitor.
const MAX_ADDRESSES: usize = 50_000;

/// One outstanding challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    /// The opaque nonce, echoed in the message.
    pub nonce: String,
    /// The exact text the client must sign.
    pub message: String,
    /// When it stops being accepted.
    pub expires_at: DateTime<Utc>,
}

/// Outstanding challenges, by address.
///
/// Up to [`MAX_PER_ADDRESS`] per address, oldest evicted first, bounded
/// overall by [`MAX_ADDRESSES`].
#[derive(Debug)]
pub struct ChallengeStore {
    entries: DashMap<Address, Vec<Challenge>>,
    ttl: Duration,
}

impl ChallengeStore {
    /// A store whose challenges live for `ttl`.
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            ttl,
        }
    }

    /// Issue a challenge for `address`.
    ///
    /// Does not evict another address's challenges, and evicts this address's
    /// own only once it holds [`MAX_PER_ADDRESS`], oldest first.
    pub fn issue(&self, address: &Address, now: DateTime<Utc>) -> Challenge {
        let mut raw = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let short = hex::encode(&raw[..5]);
        let nonce = format!("cbl-login-{}", hex::encode(raw));
        let rendered = crate::auth::eth::checksum_address(address);
        let challenge = Challenge {
            message: format!(
                "SuperFrontierLiquid wants you to sign in.\n\n\
                 Address: {rendered}\nNonce: {short}\nIssued: {}",
                now.to_rfc3339()
            ),
            nonce,
            expires_at: now + self.ttl,
        };

        if !self.entries.contains_key(address) && self.entries.len() >= MAX_ADDRESSES {
            self.make_room(now);
        }

        let mut slot = self.entries.entry(*address).or_default();
        slot.retain(|existing| existing.expires_at > now);
        slot.push(challenge.clone());
        while slot.len() > MAX_PER_ADDRESS {
            slot.remove(0);
        }
        challenge
    }

    /// Every challenge for `address` that is still valid, newest last.
    pub fn live(&self, address: &Address, now: DateTime<Utc>) -> Vec<Challenge> {
        self.entries
            .get(address)
            .map(|slot| {
                slot.iter()
                    .filter(|challenge| challenge.expires_at > now)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Retire one challenge by nonce, which is what a successful login does.
    ///
    /// Returns whether it was there. A challenge is single use: removing it
    /// here is what stops the same signature being presented twice.
    pub fn retire(&self, address: &Address, nonce: &str) -> bool {
        let mut removed = false;
        let mut empty = false;
        if let Some(mut slot) = self.entries.get_mut(address) {
            let before = slot.len();
            slot.retain(|challenge| challenge.nonce != nonce);
            removed = slot.len() != before;
            empty = slot.is_empty();
        }
        if empty {
            self.entries.remove(address);
        }
        removed
    }

    /// Drop challenges that have expired, and addresses left with none.
    pub fn prune(&self, now: DateTime<Utc>) {
        self.entries.retain(|_, slot| {
            slot.retain(|challenge| challenge.expires_at > now);
            !slot.is_empty()
        });
    }

    /// Make space when the address map is at its ceiling: expired entries
    /// first, then the addresses whose newest challenge expires soonest.
    fn make_room(&self, now: DateTime<Utc>) {
        self.prune(now);
        if self.entries.len() < MAX_ADDRESSES {
            return;
        }
        let mut by_age: Vec<(Address, DateTime<Utc>)> = self
            .entries
            .iter()
            .map(|entry| {
                let newest = entry
                    .value()
                    .iter()
                    .map(|challenge| challenge.expires_at)
                    .max()
                    .unwrap_or(now);
                (*entry.key(), newest)
            })
            .collect();
        by_age.sort_by_key(|(_, expires)| *expires);
        for (address, _) in by_age.into_iter().take(MAX_ADDRESSES / 10 + 1) {
            self.entries.remove(&address);
        }
    }

    /// How many challenges are outstanding, across every address.
    pub fn len(&self) -> usize {
        self.entries.iter().map(|entry| entry.value().len()).sum()
    }

    /// Whether any are outstanding.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many addresses have at least one outstanding challenge.
    pub fn tracked_addresses(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
            .expect("static")
            .with_timezone(&Utc)
    }

    fn store() -> ChallengeStore {
        ChallengeStore::new(Duration::minutes(5))
    }

    #[test]
    fn a_challenge_names_the_address_and_expires_in_five_minutes() {
        let store = store();
        let address = Address([0x98; 20]);
        let challenge = store.issue(&address, now());
        assert!(challenge.nonce.starts_with("cbl-login-"));
        assert!(challenge
            .message
            .contains(&crate::auth::eth::checksum_address(&address)));
        assert!(challenge.message.contains("Nonce: "));
        assert_eq!(challenge.expires_at, now() + Duration::minutes(5));
    }

    #[test]
    fn two_challenges_are_never_the_same() {
        let store = store();
        let address = Address([1; 20]);
        let first = store.issue(&address, now());
        let second = store.issue(&address, now());
        assert_ne!(first.nonce, second.nonce);
    }

    #[test]
    fn a_challenge_can_be_used_once() {
        let store = store();
        let address = Address([1; 20]);
        let issued = store.issue(&address, now());
        assert!(store.retire(&address, &issued.nonce), "first use");
        assert!(
            !store.retire(&address, &issued.nonce),
            "a replayed signature must find no challenge waiting"
        );
        assert!(store.live(&address, now()).is_empty());
    }

    #[test]
    fn a_second_issue_does_not_evict_a_challenge_someone_is_signing() {
        // Finding 7: POST /auth/challenge needs no authentication, so if a
        // second issue evicted the first, anyone who knew a victim's address
        // could keep the victim's login failing.
        let store = store();
        let address = Address([1; 20]);
        let victim = store.issue(&address, now());
        let attacker = store.issue(&address, now());
        assert_ne!(victim.nonce, attacker.nonce);

        let live = store.live(&address, now());
        assert_eq!(live.len(), 2);
        assert!(live.contains(&victim), "the in-flight challenge survives");
        assert!(store.retire(&address, &victim.nonce), "and still logs in");
        // Retiring one leaves the other usable.
        assert_eq!(store.live(&address, now()), vec![attacker]);
    }

    #[test]
    fn the_live_set_per_address_is_bounded_oldest_first() {
        let store = store();
        let address = Address([1; 20]);
        let issued: Vec<Challenge> = (0..MAX_PER_ADDRESS + 2)
            .map(|_| store.issue(&address, now()))
            .collect();
        let live = store.live(&address, now());
        assert_eq!(live.len(), MAX_PER_ADDRESS);
        assert!(
            !live.contains(&issued[0]),
            "the oldest challenge is the one evicted"
        );
        assert!(live.contains(issued.last().expect("issued")));
    }

    #[test]
    fn an_expired_challenge_is_not_accepted() {
        let store = store();
        let address = Address([1; 20]);
        let challenge = store.issue(&address, now());
        assert!(store
            .live(&address, now() + Duration::minutes(6))
            .is_empty());
        // Retiring it is still harmless.
        assert!(store.retire(&address, &challenge.nonce));
    }

    #[test]
    fn one_addresss_challenge_does_not_answer_for_another() {
        let store = store();
        let issued = store.issue(&Address([1; 20]), now());
        assert!(store.live(&Address([2; 20]), now()).is_empty());
        assert!(!store.retire(&Address([2; 20]), &issued.nonce));
    }

    #[test]
    fn expired_challenges_are_pruned() {
        let store = store();
        store.issue(&Address([1; 20]), now());
        store.issue(&Address([2; 20]), now());
        assert_eq!(store.len(), 2);
        assert_eq!(store.tracked_addresses(), 2);
        store.prune(now() + Duration::minutes(6));
        assert!(store.is_empty());
        assert_eq!(store.tracked_addresses(), 0);
    }

    #[test]
    fn the_address_map_does_not_grow_without_bound() {
        // One unauthenticated request per address, and 2^160 addresses to
        // choose from; the janitor alone is not a bound.
        let store = store();
        for i in 0..2_000u32 {
            let mut bytes = [0u8; 20];
            bytes[..4].copy_from_slice(&i.to_be_bytes());
            store.issue(&Address(bytes), now());
            assert!(store.tracked_addresses() <= MAX_ADDRESSES);
        }
        assert_eq!(store.tracked_addresses(), 2_000);
    }
}
