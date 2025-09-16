//! This module implements high-level Rust bindings for a Schnorr-based
//! multi-signature scheme called MuSig2 [paper](https://eprint.iacr.org/2020/1261).
//! It is compatible with bip-schnorr.
//!
//! The documentation in this module is for reference and may not be sufficient
//! for advanced use-cases. A full description of the C API usage along with security considerations
//! can be found in [C-musig.md](secp256k1-sys/depend/secp256k1/src/modules/musig/musig.md).
use core;
use core::fmt;
use core::mem::MaybeUninit;
#[cfg(feature = "std")]
use std;

use crate::ffi::{self, CPtr};
use crate::{
    from_hex, schnorr, Error, Keypair, PublicKey, Scalar, Secp256k1, SecretKey, XOnlyPublicKey,
};

/// Serialized size (in bytes) of the aggregated nonce.
/// The serialized form is used for transmitting or storing the aggregated nonce.
pub const AGGNONCE_SERIALIZED_SIZE: usize = 66;

/// Serialized size (in bytes) of an individual public nonce.
/// The serialized form is used for transmission between signers.
pub const PUBNONCE_SERIALIZED_SIZE: usize = 66;

/// Serialized size (in bytes) of a partial signature.
/// The serialized form is used for transmitting partial signatures to be
/// aggregated into the final signature.
pub const PART_SIG_SERIALIZED_SIZE: usize = 32;

/// Musig parsing errors
#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub enum ParseError {
    /// Parse Argument is malformed. This might occur if the point is on the secp order,
    /// or if the secp scalar is outside of group order
    MalformedArg,
}

#[cfg(feature = "std")]
impl std::error::Error for ParseError {}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            ParseError::MalformedArg => write!(f, "Malformed parse argument"),
        }
    }
}

/// Session Id for a MuSig session.
#[allow(missing_copy_implementations)]
#[derive(Debug, Eq, PartialEq)]
pub struct SessionSecretRand([u8; 32]);

impl SessionSecretRand {
    /// Creates a new [`SessionSecretRand`] with random bytes from the given rng
    #[cfg(feature = "rand")]
    pub fn from_rng<R: rand::Rng + ?Sized>(rng: &mut R) -> Self {
        let session_secrand = crate::random_32_bytes(rng);
        SessionSecretRand(session_secrand)
    }

    /// Creates a new [`SessionSecretRand`] with the given bytes.
    ///
    /// Special care must be taken that the bytes are unique for each call to
    /// [`KeyAggCache::nonce_gen`] or [`new_nonce_pair`]. The simplest
    /// recommendation is to use a cryptographicaly random 32-byte value.
    ///
    /// If the **rand** feature is enabled, [`SessionSecretRand::from_rng`] can be used to generate a
    /// random session id.
    ///
    /// # Panics
    ///
    /// Panics if passed the all-zeros string. This is disallowed by the upstream
    /// library. The input to this function should either be the whitened output of
    /// a random number generator, or if that is not available, the output of a
    /// stable monotonic counter.
    pub fn assume_unique_per_nonce_gen(inner: [u8; 32]) -> Self {
        // See SecretKey::eq for this "constant-time" algorithm for comparison against zero.
        let inner_or = inner.iter().fold(0, |accum, x| accum | *x);
        assert!(
            unsafe { core::ptr::read_volatile(&inner_or) != 0 },
            "session secrets may not be all zero",
        );

        SessionSecretRand(inner)
    }

    /// Obtains the inner bytes of the [`SessionSecretRand`].
    pub fn to_byte_array(&self) -> [u8; 32] { self.0 }

    /// Obtains a reference to the inner bytes of the [`SessionSecretRand`].
    pub fn as_byte_array(&self) -> &[u8; 32] { &self.0 }

    /// Obtains a mutable raw pointer to the beginning of the underlying storage.
    ///
    /// This is a low-level function and not exposed in the public API.
    fn as_mut_ptr(&mut self) -> *mut u8 { self.0.as_mut_ptr() }
}

///  Cached data related to a key aggregation.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyAggCache {
    data: ffi::MusigKeyAggCache,
    aggregated_xonly_public_key: XOnlyPublicKey,
}

impl CPtr for KeyAggCache {
    type Target = ffi::MusigKeyAggCache;

    fn as_c_ptr(&self) -> *const Self::Target { self.as_ptr() }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target { self.as_mut_ptr() }
}

/// Musig tweaking related error.
#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct InvalidTweakErr;

#[cfg(feature = "std")]
impl std::error::Error for InvalidTweakErr {}

impl fmt::Display for InvalidTweakErr {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "The tweak is negation of secret key")
    }
}

/// Low level API for starting a signing session by generating a nonce.
///
/// Use [`KeyAggCache::nonce_gen`] whenever
/// possible. This API provides full flexibility in providing custom nonce generation,
/// but should be use with care.
///
/// This function outputs a secret nonce that will be required for signing and a
/// corresponding public nonce that is intended to be sent to other signers.
///
/// MuSig differs from regular Schnorr signing in that implementers _must_ take
/// special care to not reuse a nonce. If you cannot provide a `sec_key`, `session_secrand`
/// UNIFORMLY RANDOM AND KEPT SECRET (even from other signers). Refer to libsecp256k1
/// documentation for additional considerations.
///
/// MuSig2 nonces can be precomputed without knowing the aggregate public key, the message to sign.
/// Refer to libsecp256k1 documentation for additional considerations.
///
/// # Arguments:
///
/// * `session_secrand`: [`SessionSecretRand`] Uniform random identifier for this session. Each call to this
///   function must have a UNIQUE `session_secrand`.
/// * `sec_key`: Optional [`SecretKey`] that we will use to sign to a create partial signature. Provide this
///   for maximal mis-use resistance.
/// * `pub_key`: [`PublicKey`] that we will use to create partial signature. The secnonce
///   output of this function cannot be used to sign for any other public key.
/// * `msg`: Optional message that will be signed later on. Provide this for maximal misuse resistance.
/// * `extra_rand`: Additional randomness for mis-use resistance. Provide this for maximal misuse resistance
///
/// Remember that nonce reuse will immediately leak the secret key!
///
/// Example:
///
/// ```rust
/// # #[cfg(feature = "std")]
/// # #[cfg(feature = "rand")] {
/// # use secp256k1::{PublicKey, SecretKey};
/// # use secp256k1::musig::{new_nonce_pair, SessionSecretRand};
/// // The session id must be sampled at random. Read documentation for more details.
/// let session_secrand = SessionSecretRand::from_rng(&mut rand::rng());
/// let sk = SecretKey::new(&mut rand::rng());
/// let pk = PublicKey::from_secret_key(&sk);
///
/// // Supply extra auxiliary randomness to prevent misuse(for example, time of day)
/// let extra_rand : Option<[u8; 32]> = None;
///
/// let (_sec_nonce, _pub_nonce) = new_nonce_pair(session_secrand, None, Some(sk), pk, None, None);
/// # }
/// ```
pub fn new_nonce_pair(
    mut session_secrand: SessionSecretRand,
    key_agg_cache: Option<&KeyAggCache>,
    sec_key: Option<SecretKey>,
    pub_key: PublicKey,
    msg: Option<&[u8; 32]>,
    extra_rand: Option<[u8; 32]>,
) -> (SecretNonce, PublicNonce) {
    let extra_ptr = extra_rand.as_ref().map(|e| e.as_ptr()).unwrap_or(core::ptr::null());
    let sk_ptr = sec_key.as_ref().map(|e| e.as_c_ptr()).unwrap_or(core::ptr::null());
    let msg_ptr = msg.as_ref().map(|e| e.as_c_ptr()).unwrap_or(core::ptr::null());
    let cache_ptr = key_agg_cache.map(|e| e.as_ptr()).unwrap_or(core::ptr::null());

    let mut seed = session_secrand.to_byte_array();
    if let Some(bytes) = sec_key {
        for (this, that) in seed.iter_mut().zip(bytes.to_secret_bytes().iter()) {
            *this ^= *that;
        }
    }
    if let Some(bytes) = extra_rand {
        for (this, that) in seed.iter_mut().zip(bytes.iter()) {
            *this ^= *that;
        }
    }

    unsafe {
        // The use of a mutable pointer to `session_secrand`, which is a local variable,
        // may seem concerning/wrong. It is ok: this pointer is only mutable because the
        // behavior of `secp256k1_musig_nonce_gen` on error is to zero out the secret
        // nonce. We guarantee this won't happen, but also if it does, it's harmless
        // to zero out a local variable without propagating that change back to the
        // caller or anything.
        let mut sec_nonce = MaybeUninit::<ffi::MusigSecNonce>::uninit();
        let mut pub_nonce = MaybeUninit::<ffi::MusigPubNonce>::uninit();

        let ret = crate::with_global_context(
            |secp: &Secp256k1<crate::AllPreallocated>| {
                ffi::secp256k1_musig_nonce_gen(
                    secp.ctx.as_ptr(),
                    sec_nonce.as_mut_ptr(),
                    pub_nonce.as_mut_ptr(),
                    session_secrand.as_mut_ptr(),
                    sk_ptr,
                    pub_key.as_c_ptr(),
                    msg_ptr,
                    cache_ptr,
                    extra_ptr,
                )
            },
            Some(&seed),
        );

        if ret == 0 {
            // Rust type system guarantees that
            // - input secret key is valid
            // - msg is 32 bytes
            // - Key agg cache is valid
            // - extra input is 32 bytes
            // This can only happen when the session id is all zeros
            panic!("A zero session id was supplied")
        } else {
            let pub_nonce = PublicNonce(pub_nonce.assume_init());
            let sec_nonce = SecretNonce(sec_nonce.assume_init());
            (sec_nonce, pub_nonce)
        }
    }
}

/// A Musig partial signature.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PartialSignature(ffi::MusigPartialSignature);

impl CPtr for PartialSignature {
    type Target = ffi::MusigPartialSignature;

    fn as_c_ptr(&self) -> *const Self::Target { self.as_ptr() }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target { self.as_mut_ptr() }
}

impl fmt::LowerHex for PartialSignature {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for b in self.serialize() {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

impl fmt::Display for PartialSignature {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { fmt::LowerHex::fmt(self, f) }
}

impl core::str::FromStr for PartialSignature {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut res = [0u8; PART_SIG_SERIALIZED_SIZE];
        match from_hex(s, &mut res) {
            Ok(PART_SIG_SERIALIZED_SIZE) => PartialSignature::from_byte_array(&res),
            _ => Err(ParseError::MalformedArg),
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for PartialSignature {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.collect_str(self)
        } else {
            s.serialize_bytes(&self.serialize()[..])
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for PartialSignature {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            d.deserialize_str(super::serde_util::FromStrVisitor::new(
                "a hex string representing a MuSig2 partial signature",
            ))
        } else {
            d.deserialize_bytes(super::serde_util::BytesVisitor::new(
                "a raw MuSig2 partial signature",
                |slice| {
                    let bytes: &[u8; PART_SIG_SERIALIZED_SIZE] =
                        slice.try_into().map_err(|_| ParseError::MalformedArg)?;

                    Self::from_byte_array(bytes)
                },
            ))
        }
    }
}

impl PartialSignature {
    /// Serialize a PartialSignature as a byte array.
    pub fn serialize(&self) -> [u8; PART_SIG_SERIALIZED_SIZE] {
        let mut data = MaybeUninit::<[u8; PART_SIG_SERIALIZED_SIZE]>::uninit();
        unsafe {
            if ffi::secp256k1_musig_partial_sig_serialize(
                ffi::secp256k1_context_no_precomp,
                data.as_mut_ptr() as *mut u8,
                self.as_ptr(),
            ) == 0
            {
                // Only fails if args are null pointer which is possible in safe rust
                unreachable!("Serialization cannot fail")
            } else {
                data.assume_init()
            }
        }
    }

    /// Deserialize a PartialSignature from bytes.
    ///
    /// # Errors:
    ///
    /// - MalformedArg: If the signature [`PartialSignature`] is out of curve order
    pub fn from_byte_array(data: &[u8; PART_SIG_SERIALIZED_SIZE]) -> Result<Self, ParseError> {
        let mut partial_sig = MaybeUninit::<ffi::MusigPartialSignature>::uninit();
        unsafe {
            if ffi::secp256k1_musig_partial_sig_parse(
                ffi::secp256k1_context_no_precomp,
                partial_sig.as_mut_ptr(),
                data.as_ptr(),
            ) == 0
            {
                Err(ParseError::MalformedArg)
            } else {
                Ok(PartialSignature(partial_sig.assume_init()))
            }
        }
    }

    /// Get a const pointer to the inner PartialSignature
    pub fn as_ptr(&self) -> *const ffi::MusigPartialSignature { &self.0 }

    /// Get a mut pointer to the inner PartialSignature
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigPartialSignature { &mut self.0 }
}

impl KeyAggCache {
    /// Creates a new [`KeyAggCache`] by supplying a list of PublicKeys used in the session.
    ///
    /// Computes a combined public key and the hash of the given public keys.
    ///
    /// Different orders of `pubkeys` result in different `agg_pk`s.
    /// The pubkeys can be sorted lexicographically before combining with which
    /// ensures the same resulting `agg_pk` for the same multiset of pubkeys.
    /// This is useful to do before aggregating pubkeys, such that the order of pubkeys
    /// does not affect the combined public key.
    /// To do this, call [`Secp256k1::sort_pubkeys`].
    ///
    /// # Returns
    ///
    ///  A [`KeyAggCache`] the can be used [`KeyAggCache::nonce_gen`] and [`Session::new`].
    ///
    /// # Args:
    ///
    /// * `secp` - Secp256k1 context object initialized for verification
    /// * `pubkeys` - Input array of public keys to combine. The order is important; a
    ///   different order will result in a different combined public key
    ///
    /// Example:
    ///
    /// ```rust
    /// # #[cfg(feature = "std")]
    /// # #[cfg(feature = "rand")] {
    /// # use secp256k1::{SecretKey, Keypair, PublicKey};
    /// # use secp256k1::musig::KeyAggCache;
    /// # let sk1 = SecretKey::new(&mut rand::rng());
    /// # let pub_key1 = PublicKey::from_secret_key(&sk1);
    /// # let sk2 = SecretKey::new(&mut rand::rng());
    /// # let pub_key2 = PublicKey::from_secret_key(&sk2);
    /// #
    /// let key_agg_cache = KeyAggCache::new(&[&pub_key1, &pub_key2]);
    /// let _agg_pk = key_agg_cache.agg_pk();
    /// # }
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if an empty slice of pubkeys is provided.
    pub fn new(pubkeys: &[&PublicKey]) -> Self {
        if pubkeys.is_empty() {
            panic!("Cannot aggregate an empty slice of pubkeys");
        }

        let mut key_agg_cache = MaybeUninit::<ffi::MusigKeyAggCache>::uninit();
        let mut agg_pk = MaybeUninit::<ffi::XOnlyPublicKey>::uninit();

        // We have no seed here but we want rerandomiziation to happen for `rand` users.
        let seed = [0_u8; 32];

        unsafe {
            let pubkeys_ref = core::slice::from_raw_parts(
                pubkeys.as_c_ptr() as *const *const ffi::PublicKey,
                pubkeys.len(),
            );

            let ret = crate::with_global_context(
                |secp: &Secp256k1<crate::AllPreallocated>| {
                    ffi::secp256k1_musig_pubkey_agg(
                        secp.ctx.as_ptr(),
                        agg_pk.as_mut_ptr(),
                        key_agg_cache.as_mut_ptr(),
                        pubkeys_ref.as_ptr(),
                        pubkeys_ref.len(),
                    )
                },
                Some(&seed),
            );
            if ret == 0 {
                // Returns 0 only if the keys are malformed that never happens in safe rust type system.
                unreachable!("Invalid XOnlyPublicKey in input pubkeys")
            } else {
                // secp256k1_musig_pubkey_agg overwrites the cache and the key so this is sound.
                let key_agg_cache = key_agg_cache.assume_init();
                let agg_pk = XOnlyPublicKey::from(agg_pk.assume_init());
                KeyAggCache { data: key_agg_cache, aggregated_xonly_public_key: agg_pk }
            }
        }
    }

    /// Obtains the aggregate public key for this [`KeyAggCache`]
    pub fn agg_pk(&self) -> XOnlyPublicKey { self.aggregated_xonly_public_key }

    /// Obtains the aggregate public key for this [`KeyAggCache`] as a full [`PublicKey`].
    ///
    /// This is only useful if you need the non-xonly public key, in particular for
    /// plain (non-xonly) tweaking or batch-verifying multiple key aggregations
    /// (not supported yet).
    pub fn agg_pk_full(&self) -> PublicKey {
        unsafe {
            let mut pk = PublicKey::from(ffi::PublicKey::new());
            if ffi::secp256k1_musig_pubkey_get(
                ffi::secp256k1_context_no_precomp,
                pk.as_mut_c_ptr(),
                self.as_ptr(),
            ) == 0
            {
                // Returns 0 only if the keys are malformed that never happens in safe rust type system.
                unreachable!("All the arguments are valid")
            } else {
                pk
            }
        }
    }

    /// Apply ordinary "EC" tweaking to a public key in a [`KeyAggCache`].
    ///
    /// This is done by adding the generator multiplied with `tweak32` to it. Returns the tweaked [`PublicKey`].
    /// This is useful for deriving child keys from an aggregate public key via BIP32.
    /// This function is required if you want to _sign_ for a tweaked aggregate key.
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for verification
    /// * `tweak`: tweak of type [`Scalar`] with which to tweak the aggregated key
    ///
    /// # Errors:
    ///
    /// If resulting public key would be invalid (only when the tweak is the negation of the corresponding
    /// secret key). For uniformly random 32-byte arrays(for example, in BIP 32 derivation) the chance of
    /// being invalid is negligible (around 1 in 2^128).
    ///
    /// Example:
    ///
    /// ```rust
    /// # #[cfg(not(secp256k1_fuzz))]
    /// # #[cfg(feature = "std")]
    /// # #[cfg(feature = "rand")] {
    /// # use secp256k1::{Scalar, SecretKey, Keypair, PublicKey};
    /// # use secp256k1::musig::KeyAggCache;
    /// # let sk1 = SecretKey::new(&mut rand::rng());
    /// # let pub_key1 = PublicKey::from_secret_key(&sk1);
    /// # let sk2 = SecretKey::new(&mut rand::rng());
    /// # let pub_key2 = PublicKey::from_secret_key(&sk2);
    /// #
    /// let mut key_agg_cache = KeyAggCache::new(&[&pub_key1, &pub_key2]);
    ///
    /// let tweak: [u8; 32] = *b"this could be a BIP32 tweak....\0";
    /// let tweak = Scalar::from_be_bytes(tweak).unwrap();
    /// let tweaked_key = key_agg_cache.pubkey_ec_tweak_add(&tweak).unwrap();
    /// # }
    /// ```
    pub fn pubkey_ec_tweak_add(&mut self, tweak: &Scalar) -> Result<PublicKey, InvalidTweakErr> {
        // We have no seed here but we want rerandomiziation to happen for `rand` users.
        let seed = [0_u8; 32];
        unsafe {
            let mut out = PublicKey::from(ffi::PublicKey::new());

            let ret = crate::with_global_context(
                |secp: &Secp256k1<crate::AllPreallocated>| {
                    ffi::secp256k1_musig_pubkey_ec_tweak_add(
                        secp.ctx.as_ptr(),
                        out.as_mut_c_ptr(),
                        self.as_mut_ptr(),
                        tweak.as_c_ptr(),
                    )
                },
                Some(&seed),
            );
            if ret == 0 {
                Err(InvalidTweakErr)
            } else {
                self.aggregated_xonly_public_key = out.x_only_public_key().0;
                Ok(out)
            }
        }
    }

    /// Apply "x-only" tweaking to a public key in a [`KeyAggCache`].
    ///
    /// This is done by adding the generator multiplied with `tweak32` to it. Returns the tweaked [`XOnlyPublicKey`].
    /// This is useful in creating taproot outputs.
    /// This function is required if you want to _sign_ for a tweaked aggregate key.
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for verification
    /// * `tweak`: tweak of type [`SecretKey`] with which to tweak the aggregated key
    ///
    /// # Errors:
    ///
    /// If resulting public key would be invalid (only when the tweak is the negation of the corresponding
    /// secret key). For uniformly random 32-byte arrays(for example, in BIP341 taproot tweaks) the chance of
    /// being invalid is negligible (around 1 in 2^128)
    ///
    /// Example:
    ///
    /// ```rust
    /// # #[cfg(not(secp256k1_fuzz))]
    /// # #[cfg(feature = "std")]
    /// # #[cfg(feature = "rand")] {
    /// # use secp256k1::{Scalar, SecretKey, Keypair, PublicKey};
    /// # use secp256k1::musig::KeyAggCache;
    /// # let sk1 = SecretKey::new(&mut rand::rng());
    /// # let pub_key1 = PublicKey::from_secret_key(&sk1);
    /// # let sk2 = SecretKey::new(&mut rand::rng());
    /// # let pub_key2 = PublicKey::from_secret_key(&sk2);
    ///
    /// let mut key_agg_cache = KeyAggCache::new(&[&pub_key1, &pub_key2]);
    ///
    /// let tweak = Scalar::from_be_bytes(*b"Insecure tweak, Don't use this!!").unwrap(); // tweak could be from tap
    /// let _x_only_key_tweaked = key_agg_cache.pubkey_xonly_tweak_add(&tweak).unwrap();
    /// # }
    /// ```
    pub fn pubkey_xonly_tweak_add(&mut self, tweak: &Scalar) -> Result<PublicKey, InvalidTweakErr> {
        // We have no seed here but we want rerandomiziation to happen for `rand` users.
        let seed = [0_u8; 32];
        unsafe {
            let mut out = PublicKey::from(ffi::PublicKey::new());

            let ret = crate::with_global_context(
                |secp: &Secp256k1<crate::AllPreallocated>| {
                    ffi::secp256k1_musig_pubkey_xonly_tweak_add(
                        secp.ctx.as_ptr(),
                        out.as_mut_c_ptr(),
                        self.as_mut_ptr(),
                        tweak.as_c_ptr(),
                    )
                },
                Some(&seed),
            );
            if ret == 0 {
                Err(InvalidTweakErr)
            } else {
                self.aggregated_xonly_public_key = out.x_only_public_key().0;
                Ok(out)
            }
        }
    }

    /// Starts a signing session by generating a nonce
    ///
    /// This function outputs a secret nonce that will be required for signing and a
    /// corresponding public nonce that is intended to be sent to other signers.
    ///
    /// MuSig differs from regular Schnorr signing in that implementers _must_ take
    /// special care to not reuse a nonce. If you cannot provide a `sec_key`, `session_secrand`
    /// UNIFORMLY RANDOM AND KEPT SECRET (even from other signers).
    /// Refer to libsecp256k1 documentation for additional considerations.
    ///
    /// MuSig2 nonces can be precomputed without knowing the aggregate public key, the message to sign.
    /// See the `new_nonce_pair` method that allows generating [`SecretNonce`] and [`PublicNonce`]
    /// with only the `session_secrand` field.
    ///
    /// If the aggregator lies, the resulting signature will simply be invalid.
    ///
    /// Remember that nonce reuse will immediately leak the secret key!
    ///
    /// # Returns:
    ///
    /// A pair of ([`SecretNonce`], [`PublicNonce`]) that can be later used signing and aggregation
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for signing
    /// * `session_secrand`: [`SessionSecretRand`] Uniform random identifier for this session. Each call to this
    ///   function must have a UNIQUE `session_secrand`.
    /// * `pub_key`: [`PublicKey`] of the signer creating the nonce.
    /// * `msg`: message that will be signed later on.
    /// * `extra_rand`: Additional randomness for mis-use resistance
    ///
    /// Example:
    ///
    /// ```rust
    /// # #[cfg(feature = "std")]
    /// # #[cfg(feature = "rand")] {
    /// # use secp256k1::{SecretKey, Keypair, PublicKey};
    /// # use secp256k1::musig::{KeyAggCache, SessionSecretRand};
    /// # let sk1 = SecretKey::new(&mut rand::rng());
    /// # let pub_key1 = PublicKey::from_secret_key(&sk1);
    /// # let sk2 = SecretKey::new(&mut rand::rng());
    /// # let pub_key2 = PublicKey::from_secret_key(&sk2);
    /// #
    /// let key_agg_cache = KeyAggCache::new(&[&pub_key1, &pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    /// let session_secrand = SessionSecretRand::from_rng(&mut rand::rng());
    ///
    /// // Provide the current time for mis-use resistance
    /// let msg = b"Public message we want to sign!!";
    /// let extra_rand : Option<[u8; 32]> = None;
    /// let (_sec_nonce, _pub_nonce) = key_agg_cache.nonce_gen(session_secrand, pub_key1, msg, extra_rand);
    /// # }
    /// ```
    pub fn nonce_gen(
        &self,
        session_secrand: SessionSecretRand,
        pub_key: PublicKey,
        msg: &[u8; 32],
        extra_rand: Option<[u8; 32]>,
    ) -> (SecretNonce, PublicNonce) {
        // The secret key here is supplied as NULL. This is okay because we supply the
        // public key and the message.
        // This makes a simple API for the user because it does not require them to pass here.
        new_nonce_pair(session_secrand, Some(self), None, pub_key, Some(msg), extra_rand)
    }

    /// Get a const pointer to the inner KeyAggCache
    pub fn as_ptr(&self) -> *const ffi::MusigKeyAggCache { &self.data }

    /// Get a mut pointer to the inner KeyAggCache
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigKeyAggCache { &mut self.data }
}

/// Musig Secret Nonce.
///
/// A signer who is online throughout the whole process and can keep this structure
/// in memory can use the provided API functions for a safe standard workflow.
///
/// This structure does not implement `Copy` or `Clone`; after construction the only
/// thing that can or should be done with this nonce is to call [`Session::partial_sign`],
/// which will take ownership. This is to prevent accidental reuse of the nonce.
///
/// See the warnings on [`Self::dangerous_into_bytes`] for more information about
/// the risks of non-standard workflows.
#[allow(missing_copy_implementations)]
#[derive(Debug)]
pub struct SecretNonce(ffi::MusigSecNonce);

impl CPtr for SecretNonce {
    type Target = ffi::MusigSecNonce;

    fn as_c_ptr(&self) -> *const Self::Target { self.as_ptr() }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target { self.as_mut_ptr() }
}

impl SecretNonce {
    /// Get a const pointer to the inner KeyAggCache
    pub fn as_ptr(&self) -> *const ffi::MusigSecNonce { &self.0 }

    /// Get a mut pointer to the inner KeyAggCache
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigSecNonce { &mut self.0 }

    /// Function to return a copy of the internal array. See WARNING before using this function.
    ///
    /// # Warning:
    ///
    /// Storing and re-creating this structure may lead to nonce reuse, which will leak
    /// your secret key in two signing sessions, even if neither session is completed.
    /// These functions should be avoided if possible and used with care.
    ///
    /// See <https://blockstream.com/2019/02/18/musig-a-new-multisignature-standard/>
    /// for more details about these risks.
    ///
    /// # Warning:
    ///
    /// The underlying library, libsecp256k1, does not guarantee the byte format will be consistent
    /// across versions or platforms. Special care should be taken to ensure the returned bytes are
    /// only ever passed to `dangerous_from_bytes` from the same libsecp256k1 version, and the same
    /// platform.
    pub fn dangerous_into_bytes(self) -> [u8; secp256k1_sys::MUSIG_SECNONCE_SIZE] {
        self.0.dangerous_into_bytes()
    }

    /// Function to create a new [`SecretNonce`] from a 32 byte array.
    ///
    /// Refer to the warnings on [`SecretNonce::dangerous_into_bytes`] for more details.
    pub fn dangerous_from_bytes(array: [u8; secp256k1_sys::MUSIG_SECNONCE_SIZE]) -> Self {
        SecretNonce(ffi::MusigSecNonce::dangerous_from_bytes(array))
    }
}

/// An individual MuSig public nonce. Not to be confused with [`AggregatedNonce`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PublicNonce(ffi::MusigPubNonce);

impl CPtr for PublicNonce {
    type Target = ffi::MusigPubNonce;

    fn as_c_ptr(&self) -> *const Self::Target { self.as_ptr() }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target { self.as_mut_ptr() }
}

impl fmt::LowerHex for PublicNonce {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for b in self.serialize() {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

impl fmt::Display for PublicNonce {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { fmt::LowerHex::fmt(self, f) }
}

impl core::str::FromStr for PublicNonce {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut res = [0u8; PUBNONCE_SERIALIZED_SIZE];
        match from_hex(s, &mut res) {
            Ok(PUBNONCE_SERIALIZED_SIZE) => PublicNonce::from_byte_array(&res),
            _ => Err(ParseError::MalformedArg),
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for PublicNonce {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.collect_str(self)
        } else {
            s.serialize_bytes(&self.serialize()[..])
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for PublicNonce {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            d.deserialize_str(super::serde_util::FromStrVisitor::new(
                "a hex string representing a MuSig2 public nonce",
            ))
        } else {
            d.deserialize_bytes(super::serde_util::BytesVisitor::new(
                "a raw MuSig2 public nonce",
                |slice| {
                    let bytes: &[u8; PUBNONCE_SERIALIZED_SIZE] =
                        slice.try_into().map_err(|_| ParseError::MalformedArg)?;

                    Self::from_byte_array(bytes)
                },
            ))
        }
    }
}

impl PublicNonce {
    /// Serialize a PublicNonce
    pub fn serialize(&self) -> [u8; PUBNONCE_SERIALIZED_SIZE] {
        let mut data = [0; PUBNONCE_SERIALIZED_SIZE];
        unsafe {
            if ffi::secp256k1_musig_pubnonce_serialize(
                ffi::secp256k1_context_no_precomp,
                data.as_mut_ptr(),
                self.as_ptr(),
            ) == 0
            {
                // Only fails when the arguments are invalid which is not possible in safe rust
                unreachable!("Arguments must be valid and well-typed")
            } else {
                data
            }
        }
    }

    /// Deserialize a PublicNonce from a portable byte representation
    ///
    /// # Errors:
    ///
    /// - MalformedArg: If the [`PublicNonce`] is 132 bytes, but out of curve order
    pub fn from_byte_array(data: &[u8; PUBNONCE_SERIALIZED_SIZE]) -> Result<Self, ParseError> {
        let mut pub_nonce = MaybeUninit::<ffi::MusigPubNonce>::uninit();
        unsafe {
            if ffi::secp256k1_musig_pubnonce_parse(
                ffi::secp256k1_context_no_precomp,
                pub_nonce.as_mut_ptr(),
                data.as_ptr(),
            ) == 0
            {
                Err(ParseError::MalformedArg)
            } else {
                Ok(PublicNonce(pub_nonce.assume_init()))
            }
        }
    }

    /// Get a const pointer to the inner PublicNonce
    pub fn as_ptr(&self) -> *const ffi::MusigPubNonce { &self.0 }

    /// Get a mut pointer to the inner PublicNonce
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigPubNonce { &mut self.0 }
}

/// Musig aggregated nonce computed by aggregating all individual public nonces
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AggregatedNonce(ffi::MusigAggNonce);

impl CPtr for AggregatedNonce {
    type Target = ffi::MusigAggNonce;

    fn as_c_ptr(&self) -> *const Self::Target { self.as_ptr() }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target { self.as_mut_ptr() }
}

impl fmt::LowerHex for AggregatedNonce {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for b in self.serialize() {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

impl fmt::Display for AggregatedNonce {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { fmt::LowerHex::fmt(self, f) }
}

impl core::str::FromStr for AggregatedNonce {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut res = [0u8; AGGNONCE_SERIALIZED_SIZE];
        match from_hex(s, &mut res) {
            Ok(AGGNONCE_SERIALIZED_SIZE) => AggregatedNonce::from_byte_array(&res),
            _ => Err(ParseError::MalformedArg),
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for AggregatedNonce {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.collect_str(self)
        } else {
            s.serialize_bytes(&self.serialize()[..])
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for AggregatedNonce {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            d.deserialize_str(super::serde_util::FromStrVisitor::new(
                "a hex string representing a MuSig2 aggregated nonce",
            ))
        } else {
            d.deserialize_bytes(super::serde_util::BytesVisitor::new(
                "a raw MuSig2 aggregated nonce",
                |slice| {
                    let bytes: &[u8; AGGNONCE_SERIALIZED_SIZE] =
                        slice.try_into().map_err(|_| ParseError::MalformedArg)?;

                    Self::from_byte_array(bytes)
                },
            ))
        }
    }
}

impl AggregatedNonce {
    /// Combine received public nonces into a single aggregated nonce
    ///
    /// This is useful to reduce the communication between signers, because instead
    /// of everyone sending nonces to everyone else, there can be one party
    /// receiving all nonces, combining the nonces with this function and then
    /// sending only the combined nonce back to the signers.
    ///
    /// Example:
    ///
    /// ```rust
    /// # #[cfg(feature = "std")]
    /// # #[cfg(feature = "rand")] {
    /// # use secp256k1::{SecretKey, Keypair, PublicKey};
    /// # use secp256k1::musig::{AggregatedNonce, KeyAggCache, SessionSecretRand};
    /// # let sk1 = SecretKey::new(&mut rand::rng());
    /// # let pub_key1 = PublicKey::from_secret_key(&sk1);
    /// # let sk2 = SecretKey::new(&mut rand::rng());
    /// # let pub_key2 = PublicKey::from_secret_key(&sk2);
    ///
    /// # let key_agg_cache = KeyAggCache::new(&[&pub_key1, &pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    ///
    /// let msg = b"Public message we want to sign!!";
    ///
    /// let session_secrand1 = SessionSecretRand::from_rng(&mut rand::rng());
    /// let (_sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(session_secrand1, pub_key1, msg, None);
    ///
    /// // Signer two does the same: Possibly on a different device
    /// let session_secrand2 = SessionSecretRand::from_rng(&mut rand::rng());
    /// let (_sec_nonce2, pub_nonce2) = key_agg_cache.nonce_gen(session_secrand2, pub_key2, msg, None);
    ///
    /// let aggnonce = AggregatedNonce::new(&[&pub_nonce1, &pub_nonce2]);
    /// # }
    /// ```
    /// # Panics
    ///
    /// Panics if an empty slice of nonces is provided.
    ///
    pub fn new(nonces: &[&PublicNonce]) -> Self {
        if nonces.is_empty() {
            panic!("Cannot aggregate an empty slice of nonces");
        }

        let mut aggnonce = MaybeUninit::<ffi::MusigAggNonce>::uninit();

        // We have no seed here but we want rerandomiziation to happen for `rand` users.
        let seed = [0_u8; 32];

        unsafe {
            let pubnonces = core::slice::from_raw_parts(
                nonces.as_c_ptr() as *const *const ffi::MusigPubNonce,
                nonces.len(),
            );

            let ret = crate::with_global_context(
                |secp: &Secp256k1<crate::AllPreallocated>| {
                    ffi::secp256k1_musig_nonce_agg(
                        secp.ctx().as_ptr(),
                        aggnonce.as_mut_ptr(),
                        pubnonces.as_ptr(),
                        pubnonces.len(),
                    )
                },
                Some(&seed),
            );
            if ret == 0 {
                // This can only crash if the individual nonces are invalid which is not possible is rust.
                // Note that even if aggregate nonce is point at infinity, the musig spec sets it as `G`
                unreachable!("Public key nonces are well-formed and valid in rust typesystem")
            } else {
                AggregatedNonce(aggnonce.assume_init())
            }
        }
    }

    /// Serialize a AggregatedNonce into a 66 bytes array.
    pub fn serialize(&self) -> [u8; AGGNONCE_SERIALIZED_SIZE] {
        let mut data = [0; AGGNONCE_SERIALIZED_SIZE];
        unsafe {
            if ffi::secp256k1_musig_aggnonce_serialize(
                ffi::secp256k1_context_no_precomp,
                data.as_mut_ptr(),
                self.as_ptr(),
            ) == 0
            {
                // Only fails when the arguments are invalid which is not possible in safe rust
                unreachable!("Arguments must be valid and well-typed")
            } else {
                data
            }
        }
    }

    /// Deserialize a AggregatedNonce from byte slice
    ///
    /// # Errors:
    ///
    /// - MalformedArg: If the byte slice is 66 bytes, but the [`AggregatedNonce`] is invalid
    pub fn from_byte_array(data: &[u8; AGGNONCE_SERIALIZED_SIZE]) -> Result<Self, ParseError> {
        let mut aggnonce = MaybeUninit::<ffi::MusigAggNonce>::uninit();
        unsafe {
            if ffi::secp256k1_musig_aggnonce_parse(
                ffi::secp256k1_context_no_precomp,
                aggnonce.as_mut_ptr(),
                data.as_ptr(),
            ) == 0
            {
                Err(ParseError::MalformedArg)
            } else {
                Ok(AggregatedNonce(aggnonce.assume_init()))
            }
        }
    }

    /// Get a const pointer to the inner AggregatedNonce
    pub fn as_ptr(&self) -> *const ffi::MusigAggNonce { &self.0 }

    /// Get a mut pointer to the inner AggregatedNonce
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigAggNonce { &mut self.0 }
}

/// The aggregated signature of all partial signatures.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AggregatedSignature([u8; 64]);

impl AggregatedSignature {
    /// Returns the aggregated signature [`schnorr::Signature`] assuming it is valid.
    ///
    /// The `partial_sig_agg` function cannot guarantee that the produced signature is valid because participants
    /// may send invalid signatures. In some applications this doesn't matter because the invalid message is simply
    /// dropped with no consequences. These can simply call this function to obtain the resulting signature. However
    /// in applications that require having valid signatures before continuing (e.g. presigned transactions in Bitcoin Lightning Network) this would be exploitable. Such applications MUST verify the resulting signature using the
    /// [`verify`](Self::verify) method.
    ///
    /// Note that while an alternative approach of verifying partial signatures is valid, verifying the aggregated
    /// signature is more performant. Thus it should be generally better to verify the signature using this function first
    /// and fall back to detection of violators if it fails.
    pub fn assume_valid(self) -> schnorr::Signature { schnorr::Signature::from_byte_array(self.0) }

    /// Verify the aggregated signature against the aggregate public key and message
    /// before returning the signature.
    pub fn verify(
        self,
        aggregate_key: &XOnlyPublicKey,
        message: &[u8],
    ) -> Result<schnorr::Signature, Error> {
        let sig = schnorr::Signature::from_byte_array(self.0);
        schnorr::verify(&sig, message, aggregate_key)
            .map(|_| sig)
            .map_err(|_| Error::IncorrectSignature)
    }
}

/// A musig Signing session.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Session(ffi::MusigSession);

impl Session {
    /// Creates a new musig signing session.
    ///
    /// Takes the public nonces of all signers and computes a session that is
    /// required for signing and verification of partial signatures.
    ///
    /// # Returns:
    ///
    /// A [`Session`] that can be later used for signing.
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for signing
    /// * `key_agg_cache`: [`KeyAggCache`] to be used for this session
    /// * `agg_nonce`: [`AggregatedNonce`], the aggregate nonce
    /// * `msg`: message that will be signed later on.
    ///
    /// Example:
    ///
    /// ```rust
    /// # #[cfg(feature = "std")]
    /// # #[cfg(feature = "rand")] {
    /// # use secp256k1::{SecretKey, Keypair, PublicKey};
    /// # use secp256k1::musig::{AggregatedNonce, KeyAggCache, Session, SessionSecretRand};
    /// # let sk1 = SecretKey::new(&mut rand::rng());
    /// # let pub_key1 = PublicKey::from_secret_key(&sk1);
    /// # let sk2 = SecretKey::new(&mut rand::rng());
    /// # let pub_key2 = PublicKey::from_secret_key(&sk2);
    ///
    /// # let key_agg_cache = KeyAggCache::new(&[&pub_key1, &pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    ///
    /// let msg = b"Public message we want to sign!!";
    ///
    /// // Provide the current time for mis-use resistance
    /// let session_secrand1 = SessionSecretRand::from_rng(&mut rand::rng());
    /// let extra_rand1 : Option<[u8; 32]> = None;
    /// let (_sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(session_secrand1, pub_key1, msg, extra_rand1);
    ///
    /// // Signer two does the same. Possibly on a different device
    /// let session_secrand2 = SessionSecretRand::from_rng(&mut rand::rng());
    /// let extra_rand2 : Option<[u8; 32]> = None;
    /// let (_sec_nonce2, pub_nonce2) = key_agg_cache.nonce_gen(session_secrand2, pub_key2, msg, extra_rand2);
    ///
    /// let aggnonce = AggregatedNonce::new(&[&pub_nonce1, &pub_nonce2]);
    ///
    /// let session = Session::new(
    ///     &key_agg_cache,
    ///     aggnonce,
    ///     msg,
    /// );
    /// # }
    /// ```
    pub fn new(key_agg_cache: &KeyAggCache, agg_nonce: AggregatedNonce, msg: &[u8; 32]) -> Self {
        let mut session = MaybeUninit::<ffi::MusigSession>::uninit();

        // We have no seed here but we want rerandomiziation to happen for `rand` users.
        let seed = [0_u8; 32];

        unsafe {
            let ret = crate::with_global_context(
                |secp: &Secp256k1<crate::AllPreallocated>| {
                    ffi::secp256k1_musig_nonce_process(
                        secp.ctx().as_ptr(),
                        session.as_mut_ptr(),
                        agg_nonce.as_ptr(),
                        msg.as_c_ptr(),
                        key_agg_cache.as_ptr(),
                    )
                },
                Some(&seed),
            );
            if ret == 0 {
                // Only fails on cryptographically unreachable codes or if the args are invalid.
                // None of which can occur in safe rust.
                unreachable!("Impossible to construct invalid arguments in safe rust.
                    Also reaches here if R1 + R2*b == point at infinity, but only occurs with 2^128 probability")
            } else {
                Session(session.assume_init())
            }
        }
    }

    /// Produces a partial signature for a given key pair and secret nonce.
    ///
    /// Remember that nonce reuse will immediately leak the secret key!
    ///
    /// # Returns:
    ///
    /// A [`PartialSignature`] that can be later be aggregated into a [`schnorr::Signature`]
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for signing
    /// * `sec_nonce`: [`SecretNonce`] to be used for this session that has never
    ///   been used before. For mis-use resistance, this API takes a mutable reference
    ///   to `sec_nonce` and sets it to zero even if the partial signing fails.
    /// * `key_pair`: The [`Keypair`] to sign the message
    /// * `key_agg_cache`: [`KeyAggCache`] containing the aggregate pubkey used in
    ///   the creation of this session
    ///
    /// # Errors:
    ///
    /// - If the provided [`SecretNonce`] has already been used for signing
    ///
    pub fn partial_sign(
        &self,
        mut secnonce: SecretNonce,
        keypair: &Keypair,
        key_agg_cache: &KeyAggCache,
    ) -> PartialSignature {
        // We have no seed here but we want rerandomiziation to happen for `rand` users.
        let seed = [0_u8; 32];
        unsafe {
            let mut partial_sig = MaybeUninit::<ffi::MusigPartialSignature>::uninit();

            let res = crate::with_global_context(
                |secp: &Secp256k1<crate::AllPreallocated>| {
                    ffi::secp256k1_musig_partial_sign(
                        secp.ctx().as_ptr(),
                        partial_sig.as_mut_ptr(),
                        secnonce.as_mut_ptr(),
                        keypair.as_c_ptr(),
                        key_agg_cache.as_ptr(),
                        self.as_ptr(),
                    )
                },
                Some(&seed),
            );

            assert_eq!(res, 1);
            PartialSignature(partial_sig.assume_init())
        }
    }

    /// Checks that an individual partial signature verifies
    ///
    /// This function is essential when using protocols with adaptor signatures.
    /// However, it is not essential for regular MuSig's, in the sense that if any
    /// partial signatures does not verify, the full signature will also not verify, so the
    /// problem will be caught. But this function allows determining the specific party
    /// who produced an invalid signature, so that signing can be restarted without them.
    ///
    /// # Returns:
    ///
    /// true if the partial signature successfully verifies, otherwise returns false
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for signing
    /// * `key_agg_cache`: [`KeyAggCache`] containing the aggregate pubkey used in
    ///   the creation of this session
    /// * `partial_sig`: [`PartialSignature`] sent by the signer associated with
    ///   the given `pub_nonce` and `pubkey`
    /// * `pub_nonce`: The [`PublicNonce`] of the signer associated with the `partial_sig`
    ///   and `pub_key`
    /// * `pub_key`: The [`XOnlyPublicKey`] of the signer associated with the given
    ///   `partial_sig` and `pub_nonce`
    ///
    /// Example:
    ///
    /// ```rust
    /// # #[cfg(not(secp256k1_fuzz))]
    /// # #[cfg(feature = "std")]
    /// # #[cfg(feature = "rand")] {
    /// # use secp256k1::{SecretKey, Keypair, PublicKey};
    /// # use secp256k1::musig::{AggregatedNonce, KeyAggCache, SessionSecretRand, Session};
    /// # let sk1 = SecretKey::new(&mut rand::rng());
    /// # let pub_key1 = PublicKey::from_secret_key(&sk1);
    /// # let sk2 = SecretKey::new(&mut rand::rng());
    /// # let pub_key2 = PublicKey::from_secret_key(&sk2);
    ///
    /// # let key_agg_cache = KeyAggCache::new(&[&pub_key1, &pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    ///
    /// let msg = b"Public message we want to sign!!";
    ///
    /// // Provide the current time for mis-use resistance
    /// let session_secrand1 = SessionSecretRand::from_rng(&mut rand::rng());
    /// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(session_secrand1, pub_key1, msg, None);
    ///
    /// // Signer two does the same. Possibly on a different device
    /// let session_secrand2 = SessionSecretRand::from_rng(&mut rand::rng());
    /// let (_sec_nonce2, pub_nonce2) = key_agg_cache.nonce_gen(session_secrand2, pub_key2, msg, None);
    ///
    /// let aggnonce = AggregatedNonce::new(&[&pub_nonce1, &pub_nonce2]);
    ///
    /// let session = Session::new(
    ///     &key_agg_cache,
    ///     aggnonce,
    ///     msg,
    /// );
    ///
    /// let keypair = Keypair::from_secret_key(&sk1);
    /// let partial_sig1 = session.partial_sign(
    ///     sec_nonce1,
    ///     &keypair,
    ///     &key_agg_cache,
    /// );
    ///
    /// assert!(session.partial_verify(
    ///     &key_agg_cache,
    ///     &partial_sig1,
    ///     &pub_nonce1,
    ///     pub_key1,
    /// ));
    /// # }
    /// ```
    pub fn partial_verify(
        &self,
        key_agg_cache: &KeyAggCache,
        partial_sig: &PartialSignature,
        pub_nonce: &PublicNonce,
        pub_key: PublicKey,
    ) -> bool {
        // We have no seed here but we want rerandomiziation to happen for `rand` users.
        let seed = [0_u8; 32];
        unsafe {
            let ret = crate::with_global_context(
                |secp: &Secp256k1<crate::AllPreallocated>| {
                    ffi::secp256k1_musig_partial_sig_verify(
                        secp.ctx.as_ptr(),
                        partial_sig.as_ptr(),
                        pub_nonce.as_ptr(),
                        pub_key.as_c_ptr(),
                        key_agg_cache.as_ptr(),
                        self.as_ptr(),
                    )
                },
                Some(&seed),
            );
            ret == 1
        }
    }

    /// Aggregate partial signatures for this session into a single [`schnorr::Signature`]
    ///
    /// # Returns:
    ///
    /// A single [`schnorr::Signature`]. Note that this does *NOT* mean that the signature verifies with respect to the
    /// aggregate public key.
    ///
    /// # Arguments:
    ///
    /// * `partial_sigs`: Array of [`PartialSignature`] to be aggregated
    ///
    /// ```rust
    /// # #[cfg(feature = "rand-std")] {
    /// # use secp256k1::{KeyAggCache, SecretKey, Keypair, PublicKey, SessionSecretRand, AggregatedNonce, Session};
    /// # let sk1 = SecretKey::new(&mut rand::rng());
    /// # let pub_key1 = PublicKey::from_secret_key(&sk1);
    /// # let sk2 = SecretKey::new(&mut rand::rng());
    /// # let pub_key2 = PublicKey::from_secret_key(&sk2);
    ///
    /// let key_agg_cache = KeyAggCache::new(&[pub_key1, pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    ///
    /// let msg = b"Public message we want to sign!!";
    ///
    /// // Provide the current time for mis-use resistance
    /// let session_secrand1 = SessionSecretRand::from_rng(&mut rand::rng());
    /// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(session_secrand1, pub_key1, msg, None)
    ///     .expect("non zero session id");
    ///
    /// // Signer two does the same. Possibly on a different device
    /// let session_secrand2 = SessionSecretRand::from_rng(&mut rand::rng());
    /// let (mut sec_nonce2, pub_nonce2) = key_agg_cache.nonce_gen(session_secrand2, pub_key2, msg, None)
    ///     .expect("non zero session id");
    ///
    /// let aggnonce = AggregatedNonce::new(&[pub_nonce1, pub_nonce2]);
    ///
    /// let session = Session::new(
    ///     &key_agg_cache,
    ///     aggnonce,
    ///     msg,
    /// );
    ///
    /// let partial_sig1 = session.partial_sign(
    ///     sec_nonce1,
    ///     &Keypair::from_secret_key(&sk1),
    ///     &key_agg_cache,
    /// ).unwrap();
    ///
    /// // Other party creates the other partial signature
    /// let partial_sig2 = session.partial_sign(
    ///     sec_nonce2,
    ///     &Keypair::from_secret_key(&sk2),
    ///     &key_agg_cache,
    /// ).unwrap();
    ///
    /// let partial_sigs = [partial_sign1, partial_sign2];
    /// let partial_sigs_ref: Vec<&PartialSignature> = partial_sigs.iter().collect();
    /// let partial_sigs_ref = partial_sigs_ref.as_slice();
    ///
    /// let aggregated_signature = session.partial_sig_agg(partial_sigs_ref);
    ///
    /// // Get the final schnorr signature
    /// assert!(aggregated_signature.verify(&agg_pk, &msg_bytes).is_ok());
    /// # }
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if an empty slice of partial signatures is provided.
    pub fn partial_sig_agg(&self, partial_sigs: &[&PartialSignature]) -> AggregatedSignature {
        if partial_sigs.is_empty() {
            panic!("Cannot aggregate an empty slice of partial signatures");
        }

        let mut sig = [0u8; 64];
        unsafe {
            let partial_sigs_ref = core::slice::from_raw_parts(
                partial_sigs.as_ptr() as *const *const ffi::MusigPartialSignature,
                partial_sigs.len(),
            );

            if ffi::secp256k1_musig_partial_sig_agg(
                ffi::secp256k1_context_no_precomp,
                sig.as_mut_ptr(),
                self.as_ptr(),
                partial_sigs_ref.as_ptr(),
                partial_sigs_ref.len(),
            ) == 0
            {
                // All arguments are well-typed partial signatures
                unreachable!("Impossible to construct invalid(not well-typed) partial signatures")
            } else {
                // Resulting signature must be well-typed. Does not mean that will be succeed verification
                AggregatedSignature(sig)
            }
        }
    }

    /// Get a const pointer to the inner Session
    pub fn as_ptr(&self) -> *const ffi::MusigSession { &self.0 }

    /// Get a mut pointer to the inner Session
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigSession { &mut self.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "std")]
    #[cfg(feature = "rand")]
    use crate::PublicKey;

    #[test]
    #[cfg(feature = "std")]
    #[cfg(feature = "rand")]
    fn session_secret_rand() {
        let mut rng = rand::rng();
        let session_secrand = SessionSecretRand::from_rng(&mut rng);
        let session_secrand1 = SessionSecretRand::from_rng(&mut rng);
        assert_ne!(session_secrand.to_byte_array(), [0; 32]); // with overwhelming probability
        assert_ne!(session_secrand, session_secrand1); // with overwhelming probability
    }

    #[test]
    fn session_secret_no_rand() {
        let custom_bytes = [42u8; 32];
        let session_secrand = SessionSecretRand::assume_unique_per_nonce_gen(custom_bytes);
        assert_eq!(session_secrand.to_byte_array(), custom_bytes);
        assert_eq!(session_secrand.as_byte_array(), &custom_bytes);
    }

    #[test]
    #[should_panic(expected = "session secrets may not be all zero")]
    fn session_secret_rand_zero_panic() {
        let zero_bytes = [0u8; 32];
        let _session_secrand = SessionSecretRand::assume_unique_per_nonce_gen(zero_bytes);
    }

    #[test]
    #[cfg(not(secp256k1_fuzz))]
    #[cfg(feature = "std")]
    fn key_agg_cache() {
        let (_seckey1, pubkey1) = crate::test_random_keypair();
        let (_seckey2, pubkey2) = crate::test_random_keypair();

        let pubkeys = [&pubkey1, &pubkey2];
        let key_agg_cache = KeyAggCache::new(&pubkeys);
        let agg_pk = key_agg_cache.agg_pk();

        // Test agg_pk_full
        let agg_pk_full = key_agg_cache.agg_pk_full();
        assert_eq!(agg_pk_full.x_only_public_key().0, agg_pk);
    }

    #[test]
    #[cfg(not(secp256k1_fuzz))]
    #[cfg(feature = "std")]
    fn key_agg_cache_tweaking() {
        let (_seckey1, pubkey1) = crate::test_random_keypair();
        let (_seckey2, pubkey2) = crate::test_random_keypair();

        let mut key_agg_cache = KeyAggCache::new(&[&pubkey1, &pubkey2]);
        let key_agg_cache1 = KeyAggCache::new(&[&pubkey2, &pubkey1]);
        let key_agg_cache2 = KeyAggCache::new(&[&pubkey1, &pubkey1]);
        let key_agg_cache3 = KeyAggCache::new(&[&pubkey1, &pubkey1, &pubkey2]);
        assert_ne!(key_agg_cache, key_agg_cache1); // swapped keys DOES mean not equal
        assert_ne!(key_agg_cache, key_agg_cache2); // missing keys
        assert_ne!(key_agg_cache, key_agg_cache3); // repeated key
        let original_agg_pk = key_agg_cache.agg_pk();
        assert_ne!(key_agg_cache.agg_pk(), key_agg_cache1.agg_pk()); // swapped keys DOES mean not equal
        assert_ne!(key_agg_cache.agg_pk(), key_agg_cache2.agg_pk()); // missing keys
        assert_ne!(key_agg_cache.agg_pk(), key_agg_cache3.agg_pk()); // repeated key

        // Test EC tweaking
        let plain_tweak: [u8; 32] = *b"this could be a BIP32 tweak....\0";
        let plain_tweak = Scalar::from_be_bytes(plain_tweak).unwrap();
        let tweaked_key = key_agg_cache.pubkey_ec_tweak_add(&plain_tweak).unwrap();
        assert_ne!(key_agg_cache.agg_pk(), original_agg_pk);
        assert_eq!(key_agg_cache.agg_pk(), tweaked_key.x_only_public_key().0);

        // Test xonly tweaking
        let xonly_tweak: [u8; 32] = *b"this could be a Taproot tweak..\0";
        let xonly_tweak = Scalar::from_be_bytes(xonly_tweak).unwrap();
        let tweaked_agg_pk = key_agg_cache.pubkey_xonly_tweak_add(&xonly_tweak).unwrap();
        assert_eq!(key_agg_cache.agg_pk(), tweaked_agg_pk.x_only_public_key().0);
    }

    #[test]
    #[cfg(feature = "std")]
    #[should_panic(expected = "Cannot aggregate an empty slice of pubkeys")]
    fn key_agg_cache_empty_panic() { let _ = KeyAggCache::new(&[]); }

    #[test]
    #[cfg(feature = "std")]
    #[cfg(feature = "rand")]
    fn nonce_generation() {
        let mut rng = rand::rng();

        let (_seckey1, pubkey1) = crate::test_random_keypair();
        let (seckey2, pubkey2) = crate::test_random_keypair();

        let key_agg_cache = KeyAggCache::new(&[&pubkey1, &pubkey2]);

        let msg: &[u8; 32] = b"This message is exactly 32 bytes";

        // Test nonce generation with KeyAggCache
        let session_secrand1 = SessionSecretRand::from_rng(&mut rng);
        let (_sec_nonce1, pub_nonce1) =
            key_agg_cache.nonce_gen(session_secrand1, pubkey1, msg, None);

        // Test direct nonce generation
        let session_secrand2 = SessionSecretRand::from_rng(&mut rng);
        let extra_rand = Some([42u8; 32]);
        let (_sec_nonce2, _pub_nonce2) = new_nonce_pair(
            session_secrand2,
            Some(&key_agg_cache),
            Some(seckey2),
            pubkey2,
            Some(msg),
            extra_rand,
        );

        // Test PublicNonce serialization/deserialization
        let serialized_nonce = pub_nonce1.serialize();
        let deserialized_nonce = PublicNonce::from_byte_array(&serialized_nonce).unwrap();
        assert_eq!(pub_nonce1.serialize(), deserialized_nonce.serialize());
    }

    #[test]
    #[cfg(feature = "std")]
    #[cfg(feature = "rand")]
    fn aggregated_nonce() {
        let mut rng = rand::rng();

        let (_seckey1, pubkey1) = crate::test_random_keypair();
        let (_seckey2, pubkey2) = crate::test_random_keypair();

        let key_agg_cache = KeyAggCache::new(&[&pubkey1, &pubkey2]);

        let msg: &[u8; 32] = b"This message is exactly 32 bytes";

        let session_secrand1 = SessionSecretRand::from_rng(&mut rng);
        let (_, pub_nonce1) = key_agg_cache.nonce_gen(session_secrand1, pubkey1, msg, None);

        let session_secrand2 = SessionSecretRand::from_rng(&mut rng);
        let (_, pub_nonce2) = key_agg_cache.nonce_gen(session_secrand2, pubkey2, msg, None);

        // Test AggregatedNonce creation
        let agg_nonce = AggregatedNonce::new(&[&pub_nonce1, &pub_nonce2]);
        let agg_nonce1 = AggregatedNonce::new(&[&pub_nonce2, &pub_nonce1]);
        let agg_nonce2 = AggregatedNonce::new(&[&pub_nonce2, &pub_nonce2]);
        let agg_nonce3 = AggregatedNonce::new(&[&pub_nonce2, &pub_nonce2]);
        assert_eq!(agg_nonce, agg_nonce1); // swapped nonces
        assert_ne!(agg_nonce, agg_nonce2); // repeated/different nonces
        assert_ne!(agg_nonce, agg_nonce3); // repeated nonce but still both nonces present

        // Test AggregatedNonce serialization/deserialization
        let serialized_agg_nonce = agg_nonce.serialize();
        let deserialized_agg_nonce =
            AggregatedNonce::from_byte_array(&serialized_agg_nonce).unwrap();
        assert_eq!(agg_nonce.serialize(), deserialized_agg_nonce.serialize());
    }

    #[test]
    #[cfg(feature = "std")]
    #[should_panic(expected = "Cannot aggregate an empty slice of nonces")]
    fn aggregated_nonce_empty_panic() {
        let empty_nonces: Vec<&PublicNonce> = vec![];
        let _agg_nonce = AggregatedNonce::new(&empty_nonces);
    }

    #[test]
    #[cfg(not(secp256k1_fuzz))]
    #[cfg(feature = "std")]
    #[cfg(feature = "rand")]
    fn session_and_partial_signing() {
        let mut rng = rand::rng();

        let (seckey1, pubkey1) = crate::test_random_keypair();
        let (seckey2, pubkey2) = crate::test_random_keypair();

        let pubkeys = [&pubkey1, &pubkey2];
        let key_agg_cache = KeyAggCache::new(&pubkeys);

        let msg: &[u8; 32] = b"This message is exactly 32 bytes";

        let session_secrand1 = SessionSecretRand::from_rng(&mut rng);
        let (sec_nonce1, pub_nonce1) =
            key_agg_cache.nonce_gen(session_secrand1, pubkey1, msg, None);

        let session_secrand2 = SessionSecretRand::from_rng(&mut rng);
        let (sec_nonce2, pub_nonce2) =
            key_agg_cache.nonce_gen(session_secrand2, pubkey2, msg, None);

        let nonces = [&pub_nonce1, &pub_nonce2];
        let agg_nonce = AggregatedNonce::new(&nonces);

        // Test Session creation
        let session = Session::new(&key_agg_cache, agg_nonce, msg);

        // Test partial signing
        let keypair1 = Keypair::from_secret_key(&seckey1);
        let partial_sign1 = session.partial_sign(sec_nonce1, &keypair1, &key_agg_cache);

        let keypair2 = Keypair::from_secret_key(&seckey2);
        let partial_sign2 = session.partial_sign(sec_nonce2, &keypair2, &key_agg_cache);

        // Test partial signature verification
        assert!(session.partial_verify(&key_agg_cache, &partial_sign1, &pub_nonce1, pubkey1));
        assert!(session.partial_verify(&key_agg_cache, &partial_sign2, &pub_nonce2, pubkey2));
        // Test that they are invalid if you switch keys
        assert!(!session.partial_verify(&key_agg_cache, &partial_sign2, &pub_nonce2, pubkey1));
        assert!(!session.partial_verify(&key_agg_cache, &partial_sign2, &pub_nonce1, pubkey2));
        assert!(!session.partial_verify(&key_agg_cache, &partial_sign2, &pub_nonce1, pubkey1));

        // Test PartialSignature serialization/deserialization
        let serialized_partial_sig = partial_sign1.serialize();
        let deserialized_partial_sig =
            PartialSignature::from_byte_array(&serialized_partial_sig).unwrap();
        assert_eq!(partial_sign1.serialize(), deserialized_partial_sig.serialize());
    }

    #[test]
    #[cfg(not(secp256k1_fuzz))]
    #[cfg(feature = "std")]
    #[cfg(feature = "rand")]
    fn signature_aggregation_and_verification() {
        let mut rng = rand::rng();

        let (seckey1, pubkey1) = crate::test_random_keypair();
        let (seckey2, pubkey2) = crate::test_random_keypair();

        let pubkeys = [&pubkey1, &pubkey2];
        let key_agg_cache = KeyAggCache::new(&pubkeys);

        let msg: &[u8; 32] = b"This message is exactly 32 bytes";

        let session_secrand1 = SessionSecretRand::from_rng(&mut rng);
        let (sec_nonce1, pub_nonce1) =
            key_agg_cache.nonce_gen(session_secrand1, pubkey1, msg, None);

        let session_secrand2 = SessionSecretRand::from_rng(&mut rng);
        let (sec_nonce2, pub_nonce2) =
            key_agg_cache.nonce_gen(session_secrand2, pubkey2, msg, None);

        let nonces = [&pub_nonce1, &pub_nonce2];
        let agg_nonce = AggregatedNonce::new(&nonces);
        let session = Session::new(&key_agg_cache, agg_nonce, msg);

        let keypair1 = Keypair::from_secret_key(&seckey1);
        let partial_sign1 = session.partial_sign(sec_nonce1, &keypair1, &key_agg_cache);

        let keypair2 = Keypair::from_secret_key(&seckey2);
        let partial_sign2 = session.partial_sign(sec_nonce2, &keypair2, &key_agg_cache);

        // Test signature verification
        let aggregated_signature = session.partial_sig_agg(&[&partial_sign1, &partial_sign2]);
        let agg_pk = key_agg_cache.agg_pk();
        aggregated_signature.verify(&agg_pk, msg).unwrap();

        // Test assume_valid
        let schnorr_sig = aggregated_signature.assume_valid();
        schnorr::verify(&schnorr_sig, msg, &agg_pk).unwrap();

        // Test with wrong aggregate (repeated sigs)
        let aggregated_signature = session.partial_sig_agg(&[&partial_sign1, &partial_sign1]);
        aggregated_signature.verify(&agg_pk, msg).unwrap_err();
        let schnorr_sig = aggregated_signature.assume_valid();
        schnorr::verify(&schnorr_sig, msg, &agg_pk).unwrap_err();

        // Test with swapped sigs -- this will work. Unlike keys, sigs are not ordered.
        let aggregated_signature = session.partial_sig_agg(&[&partial_sign2, &partial_sign1]);
        aggregated_signature.verify(&agg_pk, msg).unwrap();
        let schnorr_sig = aggregated_signature.assume_valid();
        schnorr::verify(&schnorr_sig, msg, &agg_pk).unwrap();
    }

    #[test]
    #[cfg(feature = "std")]
    #[cfg(feature = "rand")]
    #[should_panic(expected = "Cannot aggregate an empty slice of partial signatures")]
    fn partial_sig_agg_empty_panic() {
        let mut rng = rand::rng();

        let (_seckey1, pubkey1) = crate::test_random_keypair();
        let (_seckey2, pubkey2) = crate::test_random_keypair();

        let pubkeys = [pubkey1, pubkey2];
        let mut pubkeys_ref: Vec<&PublicKey> = pubkeys.iter().collect();
        let pubkeys_ref = pubkeys_ref.as_mut_slice();

        let key_agg_cache = KeyAggCache::new(pubkeys_ref);
        let msg: &[u8; 32] = b"This message is exactly 32 bytes";

        let session_secrand1 = SessionSecretRand::from_rng(&mut rng);
        let (_, pub_nonce1) = key_agg_cache.nonce_gen(session_secrand1, pubkey1, msg, None);
        let session_secrand2 = SessionSecretRand::from_rng(&mut rng);
        let (_, pub_nonce2) = key_agg_cache.nonce_gen(session_secrand2, pubkey2, msg, None);

        let nonces = [pub_nonce1, pub_nonce2];
        let nonces_ref: Vec<&PublicNonce> = nonces.iter().collect();
        let agg_nonce = AggregatedNonce::new(&nonces_ref);
        let session = Session::new(&key_agg_cache, agg_nonce, msg);

        let _agg_sig = session.partial_sig_agg(&[]);
    }

    #[test]
    fn de_serialization() {
        const MUSIG_PUBLIC_NONCE_HEX: &str = "03f4a361abd3d50535be08421dbc73b0a8f595654ae3238afcaf2599f94e25204c036ba174214433e21f5cd0fcb14b038eb40b05b7e7c820dd21aa568fdb0a9de4d7";
        let pubnonce: PublicNonce = MUSIG_PUBLIC_NONCE_HEX.parse().unwrap();

        assert_eq!(pubnonce.to_string(), MUSIG_PUBLIC_NONCE_HEX);

        const MUSIG_AGGREGATED_NONCE_HEX: &str = "0218c30fe0f567a4a9c05eb4835e2735419cf30f834c9ce2fe3430f021ba4eacd503112e97bcf6a022d236d71a9357824a2b19515f980131b3970b087cadf94cc4a7";
        let aggregated_nonce: AggregatedNonce = MUSIG_AGGREGATED_NONCE_HEX.parse().unwrap();
        assert_eq!(aggregated_nonce.to_string(), MUSIG_AGGREGATED_NONCE_HEX);

        const MUSIG_PARTIAL_SIGNATURE_HEX: &str =
            "289eeb2f5efc314aa6d87bf58125043c96d15a007db4b6aaaac7d18086f49a99";
        let partial_signature: PartialSignature = MUSIG_PARTIAL_SIGNATURE_HEX.parse().unwrap();
        assert_eq!(partial_signature.to_string(), MUSIG_PARTIAL_SIGNATURE_HEX);
    }
}
