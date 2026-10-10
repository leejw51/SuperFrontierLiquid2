//! Sign-magnitude integer helper for persisted signed values.
//!
//! Bincode happily encodes `i128`, but the moment a future revision of bincode
//! changes its zigzag/varint scheme the on-disk bytes drift. We've already paid
//! for canonical encoding once; lock signed values to explicit `(sign: i8, abs: u128)`
//! pairs in any persisted struct. In-memory math reconstitutes `i128` for
//! arithmetic and re-splits before write.

use serde::{Deserialize, Serialize};

/// A canonical sign-magnitude integer. `sign` is one of `-1`, `0`, `+1`. When
/// `sign == 0` the magnitude must be `0` — constructors enforce that invariant.
///
/// The fields stay public because the engine builds results as literals, but
/// **decoding validates**: a persisted or received value is routed through the
/// same checks as [`Signed::new`], so `(sign: 2, abs: 5)` or `(0, 5)` coming
/// off disk or off the wire is a decode error rather than a number that reads
/// as zero. A non-canonical zero, `(±1, 0)`, is refused too: nothing in the
/// workspace writes one (every constructor and every engine literal returns
/// [`Signed::ZERO`] for a zero magnitude), and accepting it would give zero two
/// encodings, so two nodes holding the same value could hash different bytes.
/// The encoding itself is unchanged; only what decodes is narrowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(try_from = "RawSigned")]
pub struct Signed {
    pub sign: i8,
    pub abs: u128,
}

impl Signed {
    pub const ZERO: Self = Self { sign: 0, abs: 0 };

    /// Build from an `(sign, abs)` pair. Normalizes `(_, 0)` to the canonical
    /// `ZERO`. Returns an error if `sign` is not in `{-1, 0, 1}`.
    pub fn new(sign: i8, abs: u128) -> Result<Self, SignedError> {
        if !matches!(sign, -1..=1) {
            return Err(SignedError::InvalidSign(sign));
        }
        if abs == 0 {
            return Ok(Self::ZERO);
        }
        if sign == 0 {
            return Err(SignedError::ZeroSignNonzeroAbs);
        }
        Ok(Self { sign, abs })
    }

    /// Reconstitute as `i128` for arithmetic.
    ///
    /// Exact for every value [`Signed::from_i128`] produces, including
    /// `i128::MIN`, whose magnitude `2^127` is one more than `i128::MAX`.
    /// A magnitude beyond what `i128` can hold saturates: to `i128::MAX` when
    /// positive, `i128::MIN` when negative. Callers that care about exactness
    /// should check beforehand.
    pub fn to_i128(self) -> i128 {
        match self.sign {
            1 => i128::try_from(self.abs).unwrap_or(i128::MAX),
            // `0 - abs` in the unsigned domain is exact down to `i128::MIN`;
            // only a magnitude past `2^127` falls back to the floor.
            -1 => 0i128.checked_sub_unsigned(self.abs).unwrap_or(i128::MIN),
            _ => 0,
        }
    }

    /// Convert from an `i128`. Always succeeds since we widen `i128::MIN.abs()`
    /// into `u128`.
    pub fn from_i128(v: i128) -> Self {
        if v == 0 {
            return Self::ZERO;
        }
        if v > 0 {
            Self {
                sign: 1,
                abs: v as u128,
            }
        } else {
            // -i128::MIN doesn't fit in i128; widen via the unsigned form.
            let abs = v.unsigned_abs();
            Self { sign: -1, abs }
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignedError {
    #[error("sign must be in {{-1, 0, 1}}, got {0}")]
    InvalidSign(i8),
    #[error("sign 0 requires abs 0")]
    ZeroSignNonzeroAbs,
    /// `(±1, 0)`: a zero written with a sign. See the type docs.
    #[error("zero must be encoded with sign 0, got sign {0}")]
    SignedZero(i8),
}

/// The wire shape of [`Signed`], decoded first and then validated.
#[derive(Deserialize)]
#[serde(rename = "Signed")]
struct RawSigned {
    sign: i8,
    abs: u128,
}

impl TryFrom<RawSigned> for Signed {
    type Error = SignedError;

    fn try_from(raw: RawSigned) -> Result<Self, SignedError> {
        if raw.abs == 0 && matches!(raw.sign, -1 | 1) {
            return Err(SignedError::SignedZero(raw.sign));
        }
        Signed::new(raw.sign, raw.abs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_canonical() {
        assert_eq!(Signed::default(), Signed::ZERO);
        assert_eq!(Signed::new(0, 0).unwrap(), Signed::ZERO);
    }

    #[test]
    fn rejects_invalid_sign() {
        assert_eq!(Signed::new(2, 1), Err(SignedError::InvalidSign(2)));
        assert_eq!(Signed::new(-2, 1), Err(SignedError::InvalidSign(-2)));
    }

    #[test]
    fn rejects_zero_sign_with_nonzero_abs() {
        assert_eq!(Signed::new(0, 5), Err(SignedError::ZeroSignNonzeroAbs));
    }

    #[test]
    fn from_i128_canonicalises() {
        assert_eq!(Signed::from_i128(0), Signed::ZERO);
        assert_eq!(Signed::from_i128(7), Signed { sign: 1, abs: 7 });
        assert_eq!(Signed::from_i128(-7), Signed { sign: -1, abs: 7 });
    }

    #[test]
    fn to_i128_round_trips() {
        for v in [-1_000_000i128, -1, 0, 1, 1_000_000].iter().copied() {
            assert_eq!(Signed::from_i128(v).to_i128(), v);
        }
    }

    #[test]
    fn from_i128_min_handles_two_complement_edge() {
        let s = Signed::from_i128(i128::MIN);
        assert_eq!(s.sign, -1);
        // i128::MIN.unsigned_abs() == 1 << 127.
        assert_eq!(s.abs, 1u128 << 127);
    }

    #[test]
    fn i128_min_round_trips_exactly() {
        // Its magnitude, 2^127, does not fit in an i128. The negative arm used
        // to saturate at -i128::MAX, one above the value it was given.
        assert_eq!(Signed::from_i128(i128::MIN).to_i128(), i128::MIN);
        assert_eq!(Signed::from_i128(i128::MIN + 1).to_i128(), i128::MIN + 1);
        assert_eq!(Signed::from_i128(i128::MAX).to_i128(), i128::MAX);
    }

    #[test]
    fn a_magnitude_beyond_i128_saturates_in_its_own_direction() {
        let big = u128::MAX;
        assert_eq!(Signed::new(1, big).unwrap().to_i128(), i128::MAX);
        assert_eq!(Signed::new(-1, big).unwrap().to_i128(), i128::MIN);
    }

    #[test]
    fn decoding_refuses_what_the_constructors_refuse() {
        // The fields are public, so these can be encoded; they must not
        // survive a trip through storage or the wire.
        let bad = [
            (Signed { sign: 2, abs: 5 }, SignedError::InvalidSign(2)),
            (Signed { sign: -3, abs: 5 }, SignedError::InvalidSign(-3)),
            (Signed { sign: 0, abs: 5 }, SignedError::ZeroSignNonzeroAbs),
            (Signed { sign: 1, abs: 0 }, SignedError::SignedZero(1)),
            (Signed { sign: -1, abs: 0 }, SignedError::SignedZero(-1)),
        ];
        for (value, why) in bad {
            let bytes = bincode::serialize(&value).unwrap();
            let err = bincode::deserialize::<Signed>(&bytes).unwrap_err();
            assert!(
                err.to_string().contains(&why.to_string()),
                "{value:?} decoded or failed for the wrong reason: {err}"
            );
        }
    }

    #[test]
    fn the_encoding_is_unchanged_by_validation() {
        // Validation narrows what decodes; it must not move a single byte of
        // what is already on disk. (sign i8, abs u128 LE), nothing else.
        let s = Signed { sign: -1, abs: 42 };
        let mut expected = vec![0xffu8];
        expected.extend_from_slice(&42u128.to_le_bytes());
        assert_eq!(bincode::serialize(&s).unwrap(), expected);
        assert_eq!(
            bincode::serialize(&Signed::ZERO).unwrap(),
            vec![0u8; 17],
            "zero is all zero bytes"
        );
    }

    #[test]
    fn every_value_from_i128_survives_encoding() {
        for v in [i128::MIN, -1, 0, 1, i128::MAX] {
            let s = Signed::from_i128(v);
            let back: Signed = bincode::deserialize(&bincode::serialize(&s).unwrap()).unwrap();
            assert_eq!(back, s);
            assert_eq!(back.to_i128(), v);
        }
    }

    #[test]
    fn bincode_round_trip() {
        let s = Signed { sign: -1, abs: 42 };
        let bytes = bincode::serialize(&s).unwrap();
        let back: Signed = bincode::deserialize(&bytes).unwrap();
        assert_eq!(s, back);
    }
}
