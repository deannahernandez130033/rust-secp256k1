// Bitcoin secp256k1 bindings
// Written in 2014 by
//   Dawid Ciężarkiewicz
//   Andrew Poelstra
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the CC0 Public Domain Dedication
// along with this software.
// If not, see <http://creativecommons.org/publicdomain/zero/1.0/>.
//

//! # Secp256k1
//! Rust bindings for Pieter Wuille's secp256k1 library, which is used for
//! fast and accurate manipulation of ECDSA signatures on the secp256k1
//! curve. Such signatures are used extensively by the Bitcoin network
//! and its derivatives.
//!

#![crate_type = "lib"]
#![crate_type = "rlib"]
#![crate_type = "dylib"]
#![crate_name = "bitcoin-secp256k1-rs"]
#![comment = "Bindings and wrapper functions for bitcoin secp256k1 library."]
#![feature(phase)]
#![feature(macro_rules)]
#![feature(globs)]  // for tests only

// Coding conventions
#![deny(non_uppercase_statics)]
#![deny(non_camel_case_types)]
#![deny(non_snake_case)]
#![deny(unused_mut)]
#![warn(missing_doc)]

extern crate "rust-crypto" as crypto;
extern crate secretdata;

extern crate libc;
extern crate serialize;
extern crate sync;
extern crate test;

use std::intrinsics::copy_nonoverlapping_memory;
use libc::c_int;
use sync::one::{Once, ONCE_INIT};

mod macros;
pub mod constants;
pub mod ffi;
pub mod key;

/// I dunno where else to put this..
fn assert_type_is_copy<T: Copy>() { }

/// A tag used for recovering the public key from a compact signature
pub struct RecoveryId(i32);

/// An ECDSA signature
pub struct Signature(uint, [u8, ..constants::MAX_SIGNATURE_SIZE]);

impl Signature {
    /// Converts the signature to a raw pointer suitable for use
    /// with the FFI functions
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        let &Signature(_, ref data) = self;
        data.as_slice().as_ptr()
    }

    /// Converts the signature to a mutable raw pointer suitable for use
    /// with the FFI functions
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        let &Signature(_, ref mut data) = self;
        data.as_mut_slice().as_mut_ptr()
    }

    /// Converts the signature to a byte slice suitable for verification
    #[inline]
    pub fn as_slice<'a>(&'a self) -> &'a [u8] {
        let &Signature(len, ref data) = self;
        data.slice_to(len)
    }

    /// Returns the length of the signature
    #[inline]
    pub fn len(&self) -> uint {
        let &Signature(len, _) = self;
        len
    }

    /// Converts a byte slice to a signature
    #[inline]
    pub fn from_slice(data: &[u8]) -> Result<Signature> {
        if data.len() <= constants::MAX_SIGNATURE_SIZE {
            let mut ret = [0, ..constants::MAX_SIGNATURE_SIZE];
            unsafe {
                copy_nonoverlapping_memory(ret.as_mut_ptr(),
                                           data.as_ptr(),
                                           data.len());
            }
            Ok(Signature(data.len(), ret))
        } else {
            Err(InvalidSignature)
        }
    }
}

/// An ECDSA error
#[deriving(PartialEq, Eq, Clone, Show)]
pub enum Error {
    /// Signature failed verification
    IncorrectSignature,
    /// Bad public key
    InvalidPublicKey,
    /// Bad signature
    InvalidSignature,
    /// Bad secret key
    InvalidSecretKey,
    /// Bad nonce
    InvalidNonce,
    /// Boolean-returning function returned the wrong boolean
    Unknown
}

/// Result type
pub type Result<T> = ::std::prelude::Result<T, Error>;

static mut Secp256k1_init : Once = ONCE_INIT;

/// Does one-time initialization of the secp256k1 engine. Can be called
/// multiple times, and is called by the `Secp256k1` constructor. This
/// only needs to be called directly if you are using the library without
/// a `Secp256k1` object, e.g. batch key generation through
/// `key::PublicKey::from_secret_key`.
pub fn init() {
    unsafe {
        Secp256k1_init.doit(|| {
            ffi::secp256k1_start();
        });
    }
}

/// Constructs a signature for `msg` using the secret key `sk` and nonce `nonce`
pub fn sign<'a>(msg: &[u8], sk: &key::SecretKey<'a>, nonce: &key::Nonce)
            -> Result<Signature> {
    let mut sig = [0, ..constants::MAX_SIGNATURE_SIZE];
    let mut len = constants::MAX_SIGNATURE_SIZE as c_int;
    unsafe {
        if ffi::secp256k1_ecdsa_sign(msg.as_ptr(), msg.len() as c_int,
                                     sig.as_mut_slice().as_mut_ptr(), &mut len,
                                     sk.as_ptr(), nonce.as_ptr()) != 1 {
            return Err(InvalidNonce);
        }
        // This assertation is probably too late :)
        assert!(len as uint <= constants::MAX_SIGNATURE_SIZE);
    };
    Ok(Signature(len as uint, sig))
}

    /// Constructs a compact signature for `msg` using the secret key `sk`
pub fn sign_compact<'a>(msg: &[u8], sk: &key::SecretKey<'a>, nonce: &key::Nonce)
                    -> Result<(Signature, RecoveryId)> {
    let mut sig = [0, ..constants::MAX_SIGNATURE_SIZE];
    let mut recid = 0;
    unsafe {
        if ffi::secp256k1_ecdsa_sign_compact(msg.as_ptr(), msg.len() as c_int,
                                             sig.as_mut_slice().as_mut_ptr(), sk.as_ptr(),
                                             nonce.as_ptr(), &mut recid) != 1 {
            return Err(InvalidNonce);
        }
    };
    Ok((Signature(constants::MAX_COMPACT_SIGNATURE_SIZE, sig), RecoveryId(recid)))
}

/// Determines the public key for which `sig` is a valid signature for
/// `msg`. Returns through the out-pointer `pubkey`.
pub fn recover_compact(msg: &[u8], sig: &[u8],
                       compressed: bool, recid: RecoveryId)
                        -> Result<key::PublicKey> {
    let mut pk = key::PublicKey::new(compressed);
    let RecoveryId(recid) = recid;

    unsafe {
        let mut len = 0;
        if ffi::secp256k1_ecdsa_recover_compact(msg.as_ptr(), msg.len() as c_int,
                                                sig.as_ptr(), pk.as_mut_ptr(), &mut len,
                                                if compressed {1} else {0},
                                                recid) != 1 {
            return Err(InvalidSignature);
        }
        assert_eq!(len as uint, pk.len());
    };
    Ok(pk)
}

/// Checks that `sig` is a valid ECDSA signature for `msg` using the public
/// key `pubkey`. Returns `Ok(true)` on success. Note that this function cannot
/// be used for Bitcoin consensus checking since there are transactions out
/// there with zero-padded signatures that don't fit in the `Signature` type.
/// Use `verify_raw` instead.
#[inline]
pub fn verify(msg: &[u8], sig: &Signature, pk: &key::PublicKey) -> Result<()> {
    verify_raw(msg, sig.as_slice(), pk)
}

/// Checks that `sig` is a valid ECDSA signature for `msg` using the public
/// key `pubkey`. Returns `Ok(true)` on success.
#[inline]
pub fn verify_raw(msg: &[u8], sig: &[u8], pk: &key::PublicKey) -> Result<()> {
    init();  // This is a static function, so we have to init
    let res = unsafe {
        ffi::secp256k1_ecdsa_verify(msg.as_ptr(), msg.len() as c_int,
                                    sig.as_ptr(), sig.len() as c_int,
                                    pk.as_ptr(), pk.len() as c_int)
    };

    match res {
        1 => Ok(()),
        0 => Err(IncorrectSignature),
        -1 => Err(InvalidPublicKey),
        -2 => Err(InvalidSignature),
        _ => unreachable!()
    }
}


#[cfg(test)]
mod tests {
    use std::rand;
    use std::rand::Rng;

    use test::{Bencher, black_box};

    use key::{SecretKey, PublicKey, Nonce};
    use super::{verify, sign, sign_compact, recover_compact};
    use super::{Signature, InvalidPublicKey, IncorrectSignature, InvalidSignature};

    #[test]
    fn invalid_pubkey() {
        let mut msg = Vec::from_elem(32, 0u8);
        let sig = Signature::from_slice([0, ..72]).unwrap();
        let pk = PublicKey::new(true);

        rand::task_rng().fill_bytes(msg.as_mut_slice());

        assert_eq!(verify(msg.as_mut_slice(), &sig, &pk), Err(InvalidPublicKey));
    }

    #[test]
    fn valid_pubkey_uncompressed() {
        let mut sk = SecretKey::new();
        sk.init_rng(&mut rand::task_rng());
        let pk = PublicKey::from_secret_key(&sk, false);

        let mut msg = Vec::from_elem(32, 0u8);
        let sig = Signature::from_slice([0, ..72]).unwrap();

        rand::task_rng().fill_bytes(msg.as_mut_slice());

        assert_eq!(verify(msg.as_mut_slice(), &sig, &pk), Err(InvalidSignature));
    }

    #[test]
    fn valid_pubkey_compressed() {
        let mut sk = SecretKey::new();
        sk.init_rng(&mut rand::task_rng());
        let pk = PublicKey::from_secret_key(&sk, true);

        let mut msg = Vec::from_elem(32, 0u8);
        let sig = Signature::from_slice([0, ..72]).unwrap();

        rand::task_rng().fill_bytes(msg.as_mut_slice());

        assert_eq!(verify(msg.as_mut_slice(), &sig, &pk), Err(InvalidSignature));
    }

    #[test]
    fn sign_random() {
        let mut rng = rand::task_rng();

        let mut sk = SecretKey::new();
        sk.init_rng(&mut rng);

        let mut msg = [0u8, ..32];
        rng.fill_bytes(msg);

        let nonce = Nonce::new(&mut rng);

        sign(msg.as_slice(), &sk, &nonce).unwrap();
    }

    #[test]
    fn sign_and_verify() {
        let mut rng = rand::task_rng();

        let mut sk = SecretKey::new();
        sk.init_rng(&mut rng);
        let pk = PublicKey::from_secret_key(&sk, true);
        let mut msg = [0u8, ..32];
        rng.fill_bytes(msg);
        let nonce = Nonce::new(&mut rng);

        let sig = sign(msg.as_slice(), &sk, &nonce).unwrap();
        assert_eq!(verify(msg.as_slice(), &sig, &pk), Ok(()));
    }

    #[test]
    fn sign_and_verify_fail() {
        let mut rng = rand::task_rng();

        let mut sk = SecretKey::new();
        sk.init_rng(&mut rng);
        let pk = PublicKey::from_secret_key(&sk, true);
        let mut msg = [0u8, ..32];
        rng.fill_bytes(msg);
        let nonce = Nonce::new(&mut rng);

        let sig = sign(msg.as_slice(), &sk, &nonce).unwrap();
        rng.fill_bytes(msg.as_mut_slice());
        assert_eq!(verify(msg.as_slice(), &sig, &pk), Err(IncorrectSignature));
    }

    #[test]
    fn sign_compact_with_recovery() {
        let mut rng = rand::task_rng();

        let mut sk = SecretKey::new();
        sk.init_rng(&mut rng);
        assert!(sk != SecretKey::new());
        let pk = PublicKey::from_secret_key(&sk, false);
        let pk_comp = PublicKey::from_secret_key(&sk, true);
        let mut msg = [0u8, ..32];
        rng.fill_bytes(msg);
        let nonce = Nonce::new(&mut rng);

        let (sig, recid) = sign_compact(msg.as_slice(), &sk, &nonce).unwrap();

        assert_eq!(recover_compact(msg.as_slice(), sig.as_slice(), false, recid), Ok(pk));
        assert_eq!(recover_compact(msg.as_slice(), sig.as_slice(), true, recid), Ok(pk_comp));
    }

    #[test]
    fn deterministic_sign() {
        let mut rng = rand::task_rng();

        let mut sk = SecretKey::new();
        sk.init_rng(&mut rng);
        let pk = PublicKey::from_secret_key(&sk, true);
        let mut msg = [0u8, ..32];
        rng.fill_bytes(msg);
        let nonce = Nonce::deterministic(msg, &sk);

        let sig = sign(msg.as_slice(), &sk, &nonce).unwrap();
        assert_eq!(verify(msg.as_slice(), &sig, &pk), Ok(()));
    }

    #[bench]
    pub fn generate_compressed(bh: &mut Bencher) {
        let mut rng = rand::task_rng();
        let mut sk = SecretKey::new();
        bh.iter( || {
          sk.init_rng(&mut rng);
          black_box(PublicKey::from_secret_key(&sk, true));
        });
    }

    #[bench]
    pub fn generate_uncompressed(bh: &mut Bencher) {
        let mut rng = rand::task_rng();
        let mut sk = SecretKey::new();
        bh.iter( || {
          sk.init_rng(&mut rng);
          black_box(PublicKey::from_secret_key(&sk, false));
        });
    }
}
