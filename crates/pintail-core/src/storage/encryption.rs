// Ported from duckdb/src/common/encryption_types.cpp, encryption_functions.cpp, encryption_key_manager.cpp, the
// encryption of duckdb/src/storage/single_file_block_manager.cpp and the AES, GCM and CTR of mbedtls (through
// duckdb/third_party/mbedtls/mbedtls_wrapper.cpp) @ rust-port-baseline
//
//! Encrypted database files (`ENCRYPTION_KEY`). The key a file is encrypted with is the SHA-256 of its database
//! identifier and the key given (base64-decoded if it decodes). The main header stays as it is, and says how the file
//! is encrypted; a canary, the encryption of `DUCKKEY`, tells a wrong key. Every other block, the database headers
//! included, keeps a nonce and a tag in front and has its checksum and data encrypted with AES-256, in GCM (with a tag)
//! or CTR (without). Files of encryption version 0 (DuckDB 1.4) have nonces of 16 bytes, which GCM hashes into its
//! counter; later ones have 12, which `ring` takes for GCM.

use pintail_common::{Error, ErrorKind, Result};
use sha2::Digest;
use std::sync::OnceLock;

/// `MainHeader::DEFAULT_ENCRYPTION_KEY_LENGTH`.
pub const KEY_LENGTH: usize = 32;
/// `MainHeader::AES_NONCE_LEN`.
pub const NONCE_LEN: usize = 12;
/// `MainHeader::AES_NONCE_LEN_DEPRECATED`: the nonces of version 0.
pub const NONCE_LEN_DEPRECATED: usize = 16;
/// `MainHeader::AES_TAG_LEN`.
pub const TAG_LEN: usize = 16;
/// `DEFAULT_ENCRYPTION_BLOCK_HEADER_SIZE`: the nonce, the tag and the checksum in front of a block.
pub const ENCRYPTED_BLOCK_HEADER_SIZE: usize = 40;
/// `Storage::DEFAULT_BLOCK_HEADER_SIZE`: the checksum.
const CHECKSUM_SIZE: usize = 8;
/// `MainHeader::CANARY`, with its terminating zero.
pub const CANARY: &[u8; 8] = b"DUCKKEY\0";
/// `EncryptionTypes::KeyDerivationFunction::SHA256`.
const KDF_SHA256: u8 = 1;

/// `EncryptionTypes::CipherType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cipher {
    Gcm = 1,
    Ctr = 2,
}

impl Cipher {
    pub fn from_number(number: u8) -> Option<Self> {
        match number {
            1 => Some(Self::Gcm),
            2 => Some(Self::Ctr),
            _ => None,
        }
    }

    /// `EncryptionTypes::CipherToString`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Gcm => "GCM",
            Self::Ctr => "CTR",
        }
    }

    /// `StorageOptions::Initialize` of `ENCRYPTION_CIPHER` (`EncryptionTypes::StringToCipher`).
    pub fn parse(text: &str) -> Result<Self> {
        match text.to_uppercase().as_str() {
            "GCM" => Ok(Self::Gcm),
            "CTR" => Ok(Self::Ctr),
            "CBC" => Err(Error::new(ErrorKind::NotImplemented, "CBC encryption is disabled")),
            _ => Err(Error::new(
                ErrorKind::Binder,
                format!("\"{text}\" is not a valid cipher. Try 'GCM' or 'CTR'."),
            )),
        }
    }
}

/// `EncryptionTypes::EncryptionVersion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
    /// DuckDB 1.4: nonces of 16 bytes, and a canary without a tag
    V0 = 0,
    V1 = 1,
}

impl Version {
    pub fn from_number(number: u8) -> Option<Self> {
        match number {
            0 => Some(Self::V0),
            1 => Some(Self::V1),
            _ => None,
        }
    }

    /// `EncryptionTypes::StringToVersion`.
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "v0" => Ok(Self::V0),
            "v1" => Ok(Self::V1),
            _ => Err(Error::new(
                ErrorKind::NotImplemented,
                "No encryption version higher then v1 is supported yet in this DuckDB version",
            )),
        }
    }

    /// `EncryptionNonce::size`: what a block or log entry stores of its nonce.
    fn nonce_len(self) -> usize {
        match self {
            Self::V0 => NONCE_LEN_DEPRECATED,
            Self::V1 => NONCE_LEN,
        }
    }
}

/// `EncryptionKeyManager::DeriveKey`: the SHA-256 of the salt (the database identifier) and the key, which is taken
/// as base64 if it decodes as that.
pub fn derive_key(user_key: &[u8], salt: &[u8]) -> [u8; KEY_LENGTH] {
    let decoded = std::str::from_utf8(user_key)
        .ok()
        .and_then(|text| crate::function::decode_base64(text).ok());
    let key = decoded.as_deref().unwrap_or(user_key);
    let mut hasher = sha2::Sha256::new();
    hasher.update(salt);
    hasher.update(key);
    hasher.finalize().into()
}

/// `RandomEngine::RandomData` of a nonce or an identifier.
pub fn random_bytes(target: &mut [u8]) -> Result<()> {
    use ring::rand::SecureRandom;
    ring::rand::SystemRandom::new()
        .fill(target)
        .map_err(|_| Error::new(ErrorKind::Internal, "no random bytes for encryption"))
}

fn tag_mismatch() -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "Computed AES tag differs from read AES tag, are you using the right key?",
    )
}

/// What `ATTACH` asks of the encryption of a file: the key, the cipher if it names one, and the version a new file
/// gets.
#[derive(Clone)]
pub struct EncryptionRequest {
    pub key: Vec<u8>,
    pub cipher: Option<Cipher>,
    pub version: Version,
    /// `force_mbedtls_unsafe`: whether files can be written encrypted. C++ writes them with the OpenSSL of httpfs,
    /// and without it only reads them, unless told to write them with its own Mbed TLS.
    pub writable: bool,
}

/// `DatabaseInstance::GetEncryptionUtil` for writing, without httpfs or `force_mbedtls_unsafe`.
pub fn read_only_crypto() -> Error {
    Error::new(
        ErrorKind::InvalidConfiguration,
        " DuckDB currently has a read-only crypto module loaded. Please ensure httpfs is loaded using `LOAD httpfs`, \
         or for DuckDB database files consider READONLY mode. To write an encrypted database or parquet file that is \
         NOT securely encrypted, one can use SET force_mbedtls_unsafe = 'true'.",
    )
}

impl std::fmt::Debug for EncryptionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EncryptionRequest({:?}, {:?})", self.cipher, self.version)
    }
}

impl Drop for EncryptionRequest {
    fn drop(&mut self) {
        self.key.fill(0);
    }
}

/// `EncryptionEngine::AddTempKeyToCache`: the random key of this process's temporary files, which go with it, under
/// GCM (`EncryptTemporaryBuffer`).
pub fn temp_key() -> Result<&'static FileEncryption> {
    static KEY: OnceLock<FileEncryption> = OnceLock::new();
    if let Some(key) = KEY.get() {
        return Ok(key);
    }
    let mut key = [0; KEY_LENGTH];
    random_bytes(&mut key)?;
    Ok(KEY.get_or_init(|| FileEncryption::new(key, Cipher::Gcm, Version::V1)))
}

/// How a file is encrypted: the key derived for it, the cipher and the version.
#[derive(Clone)]
pub struct FileEncryption {
    aes: Aes256,
    gcm: ring::aead::LessSafeKey,
    key: [u8; KEY_LENGTH],
    pub cipher: Cipher,
    pub version: Version,
}

impl std::fmt::Debug for FileEncryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileEncryption({}, {:?})", self.cipher.name(), self.version)
    }
}

impl Drop for FileEncryption {
    /// `EncryptionKey::~EncryptionKey`: the key goes when nothing uses it.
    fn drop(&mut self) {
        self.key.fill(0);
    }
}

impl FileEncryption {
    pub fn new(key: [u8; KEY_LENGTH], cipher: Cipher, version: Version) -> Self {
        let unbound = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key).expect("a key of 32 bytes");
        Self {
            aes: Aes256::new(&key),
            gcm: ring::aead::LessSafeKey::new(unbound),
            key,
            cipher,
            version,
        }
    }

    /// The bytes `data` encrypts to in place under `cipher` with this nonce (of its whole length: CTR's of version 1
    /// has 4 bytes of counter after the 12 stored), with the tag GCM gives.
    fn seal(&self, cipher: Cipher, nonce: &[u8], data: &mut [u8]) -> [u8; TAG_LEN] {
        match cipher {
            Cipher::Ctr => {
                ctr_full(&self.aes, nonce.try_into().expect("a counter block of 16 bytes"), data);
                [0; TAG_LEN]
            }
            Cipher::Gcm if nonce.len() == NONCE_LEN => {
                let nonce = ring::aead::Nonce::try_assume_unique_for_key(nonce).expect("a nonce of 12 bytes");
                let tag = self
                    .gcm
                    .seal_in_place_separate_tag(nonce, ring::aead::Aad::empty(), data)
                    .expect("a block within the limits of GCM");
                tag.as_ref().try_into().expect("a tag of 16 bytes")
            }
            Cipher::Gcm => gcm_seal(&self.aes, nonce, data),
        }
    }

    /// The inverse of [`Self::seal`], checking GCM's tag.
    fn open(&self, cipher: Cipher, nonce: &[u8], data: &mut [u8], tag: &[u8; TAG_LEN]) -> Result<()> {
        match cipher {
            Cipher::Ctr => {
                ctr_full(&self.aes, nonce.try_into().expect("a counter block of 16 bytes"), data);
                Ok(())
            }
            Cipher::Gcm if nonce.len() == NONCE_LEN => {
                let nonce = ring::aead::Nonce::try_assume_unique_for_key(nonce).expect("a nonce of 12 bytes");
                let tag = ring::aead::Tag::from(*tag);
                self.gcm
                    .open_in_place_separate_tag(nonce, ring::aead::Aad::empty(), tag, data, 0..)
                    .map(|_| ())
                    .map_err(|_| tag_mismatch())
            }
            Cipher::Gcm => {
                let computed = gcm_seal_inverse(&self.aes, nonce, data);
                if computed != *tag {
                    return Err(tag_mismatch());
                }
                Ok(())
            }
        }
    }

    /// `EncryptionNonce` of this file's cipher and version: the nonce as used, from what is stored of it.
    fn full_nonce(&self, stored: &[u8]) -> Vec<u8> {
        let mut nonce = stored.to_vec();
        // CTR's nonce of version 1 is the counter block, with 4 bytes of counter at 0
        if self.cipher == Cipher::Ctr && self.version == Version::V1 {
            nonce.resize(16, 0);
        }
        nonce
    }

    /// `EncryptionEngine::EncryptBlock`: `block` is a whole block of the file, the encryption header in front, with
    /// its checksum at [`ENCRYPTED_BLOCK_HEADER_SIZE`] - 8 already; this encrypts the checksum and the data, and puts
    /// the nonce and tag in front.
    pub fn encrypt_block(&self, block: &mut [u8]) -> Result<()> {
        let nonce_len = self.version.nonce_len();
        let mut stored = vec![0; nonce_len];
        random_bytes(&mut stored)?;
        let nonce = self.full_nonce(&stored);
        let delta = ENCRYPTED_BLOCK_HEADER_SIZE - CHECKSUM_SIZE;
        let tag = self.seal(self.cipher, &nonce, &mut block[delta..]);
        block[..delta].fill(0);
        block[..nonce_len].copy_from_slice(&stored);
        block[nonce_len..nonce_len + TAG_LEN].copy_from_slice(&tag);
        Ok(())
    }

    /// `EncryptionEngine::DecryptBlock`: the inverse of [`Self::encrypt_block`], in place.
    pub fn decrypt_block(&self, block: &mut [u8]) -> Result<()> {
        let nonce_len = self.version.nonce_len();
        let nonce = self.full_nonce(&block[..nonce_len]);
        let tag: [u8; TAG_LEN] = block[nonce_len..nonce_len + TAG_LEN].try_into().expect("16 bytes");
        let delta = ENCRYPTED_BLOCK_HEADER_SIZE - CHECKSUM_SIZE;
        self.open(self.cipher, &nonce, &mut block[delta..], &tag)
    }

    /// `ChecksumWriter::FlushEncrypted`: an entry of the log as it goes into the file, after its size: the nonce, the
    /// encrypted checksum and entry, and GCM's tag.
    pub fn encrypt_log_entry(&self, checksum: u64, entry: &[u8]) -> Result<Vec<u8>> {
        let nonce_len = self.version.nonce_len();
        let mut stored = vec![0; nonce_len];
        random_bytes(&mut stored)?;
        let nonce = self.full_nonce(&stored);
        let mut data = Vec::with_capacity(CHECKSUM_SIZE + entry.len());
        data.extend_from_slice(&checksum.to_le_bytes());
        data.extend_from_slice(entry);
        let tag = self.seal(self.cipher, &nonce, &mut data);
        let mut result = stored;
        result.extend_from_slice(&data);
        if self.cipher == Cipher::Gcm {
            result.extend_from_slice(&tag);
        }
        Ok(result)
    }

    /// What follows the size of an encrypted entry of `size` bytes: the nonce, checksum and tag around it.
    pub fn log_entry_overhead(&self) -> usize {
        self.version.nonce_len() + CHECKSUM_SIZE + if self.cipher == Cipher::Gcm { TAG_LEN } else { 0 }
    }

    /// `ReplayInternal` of a log of version 3: the stored checksum and the entry of what follows the size of one.
    pub fn decrypt_log_entry(&self, framed: &[u8]) -> Result<(u64, Vec<u8>)> {
        let nonce_len = self.version.nonce_len();
        let tag_len = if self.cipher == Cipher::Gcm { TAG_LEN } else { 0 };
        let nonce = self.full_nonce(&framed[..nonce_len]);
        let mut data = framed[nonce_len..framed.len() - tag_len].to_vec();
        let mut tag = [0; TAG_LEN];
        if tag_len > 0 {
            tag.copy_from_slice(&framed[framed.len() - TAG_LEN..]);
        }
        self.open(self.cipher, &nonce, &mut data, &tag)?;
        let checksum = u64::from_le_bytes(data[..CHECKSUM_SIZE].try_into().expect("8 bytes"));
        Ok((checksum, data.split_off(CHECKSUM_SIZE)))
    }

    /// `EncryptCanary`: the encrypted canary, its nonce and its tag, for the main header. From version 1 on the canary
    /// is GCM's with a nonce of its own; before, it is the file's cipher's with a nonce of zeros, and no tag.
    pub fn canary(&self) -> Result<([u8; 8], [u8; NONCE_LEN], [u8; TAG_LEN])> {
        let mut canary = *CANARY;
        match self.version {
            Version::V0 => {
                let nonce = [0; NONCE_LEN_DEPRECATED];
                self.seal(self.cipher, &nonce, &mut canary);
                Ok((canary, [0; NONCE_LEN], [0; TAG_LEN]))
            }
            Version::V1 => {
                let mut nonce = [0; NONCE_LEN];
                random_bytes(&mut nonce)?;
                let tag = self.seal(Cipher::Gcm, &nonce, &mut canary);
                Ok((canary, nonce, tag))
            }
        }
    }

    /// `DecryptCanary`: whether this is the key the canary was encrypted with. A canary of version 0 has no tag, so
    /// only what it decrypts to tells.
    pub fn check_canary(&self, canary: &[u8; 8], nonce: &[u8; NONCE_LEN], tag: &[u8; TAG_LEN]) -> bool {
        let mut decrypted = *canary;
        match (self.version, self.cipher) {
            (Version::V0, Cipher::Gcm) => {
                gcm_seal_inverse(&self.aes, &[0; NONCE_LEN_DEPRECATED], &mut decrypted);
            }
            (Version::V0, Cipher::Ctr) => ctr_full(&self.aes, [0; 16], &mut decrypted),
            (Version::V1, _) => {
                if self.open(Cipher::Gcm, nonce, &mut decrypted, tag).is_err() {
                    return false;
                }
            }
        }
        &decrypted == CANARY
    }

    /// `SingleFileBlockManager::StoreEncryptionMetadata`: the key derivation function, whether there is additional
    /// authenticated data, the cipher, the version and the length of the key.
    pub fn metadata(&self) -> [u8; 8] {
        let mut metadata = [0; 8];
        metadata[0] = KDF_SHA256;
        metadata[2] = self.cipher as u8;
        metadata[3] = self.version as u8;
        metadata[4..8].copy_from_slice(&(KEY_LENGTH as u32).to_le_bytes());
        metadata
    }
}

/// AES-256 encryption, of the T-tables of the reference implementation: what CTR and GCM need of it.
#[derive(Clone)]
struct Aes256 {
    round_keys: [u32; 60],
}

const ROUNDS: usize = 14;

const SBOX: [u8; 256] = {
    let mut sbox = [0u8; 256];
    // the multiplicative inverse in GF(2^8), then the affine map
    let mut p: u8 = 1;
    let mut q: u8 = 1;
    loop {
        // p * 3
        p = p ^ (p << 1) ^ if p & 0x80 != 0 { 0x1b } else { 0 };
        // q / 3
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        if q & 0x80 != 0 {
            q ^= 0x09;
        }
        let x = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4);
        sbox[p as usize] = x ^ 0x63;
        if p == 1 {
            break;
        }
    }
    sbox[0] = 0x63;
    sbox
};

fn xtime(x: u8) -> u8 {
    (x << 1) ^ if x & 0x80 != 0 { 0x1b } else { 0 }
}

/// `Te0`: a column of MixColumns of the S-box value, the other three being its rotations.
fn te0() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let s = SBOX[i];
            let s2 = xtime(s);
            let s3 = s2 ^ s;
            *entry = u32::from_be_bytes([s2, s, s, s3]);
        }
        table
    })
}

impl Aes256 {
    fn new(key: &[u8; KEY_LENGTH]) -> Self {
        let mut w = [0u32; 60];
        for (i, word) in w.iter_mut().take(8).enumerate() {
            *word = u32::from_be_bytes(key[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        }
        let sub = |word: u32| {
            let b = word.to_be_bytes();
            u32::from_be_bytes([
                SBOX[b[0] as usize],
                SBOX[b[1] as usize],
                SBOX[b[2] as usize],
                SBOX[b[3] as usize],
            ])
        };
        let mut rcon: u8 = 1;
        for i in 8..60 {
            let mut temp = w[i - 1];
            if i % 8 == 0 {
                temp = sub(temp.rotate_left(8)) ^ (u32::from(rcon) << 24);
                rcon = xtime(rcon);
            } else if i % 8 == 4 {
                temp = sub(temp);
            }
            w[i] = w[i - 8] ^ temp;
        }
        Self { round_keys: w }
    }

    fn encrypt(&self, block: &mut [u8; 16]) {
        let te = te0();
        let k = &self.round_keys;
        let mut s = [0u32; 4];
        for (i, word) in s.iter_mut().enumerate() {
            *word = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().expect("4 bytes")) ^ k[i];
        }
        let byte = |x: u32, n: u32| ((x >> (24 - 8 * n)) & 0xff) as usize;
        for round in 1..ROUNDS {
            let mut t = [0u32; 4];
            for (c, out) in t.iter_mut().enumerate() {
                *out = te[byte(s[c], 0)]
                    ^ te[byte(s[(c + 1) % 4], 1)].rotate_right(8)
                    ^ te[byte(s[(c + 2) % 4], 2)].rotate_right(16)
                    ^ te[byte(s[(c + 3) % 4], 3)].rotate_right(24)
                    ^ k[round * 4 + c];
            }
            s = t;
        }
        for c in 0..4 {
            let word = u32::from_be_bytes([
                SBOX[byte(s[c], 0)],
                SBOX[byte(s[(c + 1) % 4], 1)],
                SBOX[byte(s[(c + 2) % 4], 2)],
                SBOX[byte(s[(c + 3) % 4], 3)],
            ]) ^ k[ROUNDS * 4 + c];
            block[c * 4..c * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
    }
}

/// XORs `data` with the key stream of `counter` on, the counter growing by one per block in `increment`.
fn ctr_with(aes: &Aes256, mut counter: [u8; 16], data: &mut [u8], increment: fn(&mut [u8; 16])) {
    for chunk in data.chunks_mut(16) {
        let mut stream = counter;
        aes.encrypt(&mut stream);
        for (byte, key) in chunk.iter_mut().zip(stream) {
            *byte ^= key;
        }
        increment(&mut counter);
    }
}

/// `mbedtls_aes_crypt_ctr`: the counter is all 16 bytes, big-endian.
fn ctr_full(aes: &Aes256, counter: [u8; 16], data: &mut [u8]) {
    ctr_with(aes, counter, data, |counter| {
        for byte in counter.iter_mut().rev() {
            *byte = byte.wrapping_add(1);
            if *byte != 0 {
                break;
            }
        }
    });
}

/// GCM's `inc32`: the last 4 bytes, big-endian, wrapping.
fn inc32(counter: &mut [u8; 16]) {
    let low = u32::from_be_bytes(counter[12..16].try_into().expect("4 bytes")).wrapping_add(1);
    counter[12..16].copy_from_slice(&low.to_be_bytes());
}

/// Multiplication in GCM's field.
fn gf_mul(x: u128, y: u128) -> u128 {
    const R: u128 = 0xe1 << 120;
    let mut z = 0u128;
    let mut v = y;
    for i in 0..128 {
        if (x >> (127 - i)) & 1 == 1 {
            z ^= v;
        }
        v = if v & 1 == 1 { (v >> 1) ^ R } else { v >> 1 };
    }
    z
}

/// GHASH of `data`, padded with zeros to whole blocks, from `state` on.
fn ghash(h: u128, mut state: u128, data: &[u8]) -> u128 {
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        state = gf_mul(state ^ u128::from_be_bytes(block), h);
    }
    state
}

/// GCM's `J0` of a nonce, and `H`.
fn gcm_start(aes: &Aes256, nonce: &[u8]) -> ([u8; 16], u128) {
    let mut zero = [0u8; 16];
    aes.encrypt(&mut zero);
    let h = u128::from_be_bytes(zero);
    let j0 = if nonce.len() == NONCE_LEN {
        let mut j0 = [0u8; 16];
        j0[..NONCE_LEN].copy_from_slice(nonce);
        j0[15] = 1;
        j0
    } else {
        let state = ghash(h, 0, nonce);
        let lengths = (nonce.len() as u128) * 8;
        gf_mul(state ^ lengths, h).to_be_bytes()
    };
    (j0, h)
}

/// GCM's tag of `ciphertext` (without additional data).
fn gcm_tag(aes: &Aes256, j0: [u8; 16], h: u128, ciphertext: &[u8]) -> [u8; TAG_LEN] {
    let state = ghash(h, 0, ciphertext);
    let lengths = (ciphertext.len() as u128) * 8;
    let state = gf_mul(state ^ lengths, h);
    let mut mask = j0;
    aes.encrypt(&mut mask);
    (state ^ u128::from_be_bytes(mask)).to_be_bytes()
}

/// GCM encryption in place for a nonce of any length, giving the tag.
fn gcm_seal(aes: &Aes256, nonce: &[u8], data: &mut [u8]) -> [u8; TAG_LEN] {
    let (j0, h) = gcm_start(aes, nonce);
    let mut counter = j0;
    inc32(&mut counter);
    ctr_with(aes, counter, data, inc32);
    gcm_tag(aes, j0, h, data)
}

/// GCM decryption in place for a nonce of any length, giving the tag the ciphertext had.
fn gcm_seal_inverse(aes: &Aes256, nonce: &[u8], data: &mut [u8]) -> [u8; TAG_LEN] {
    let (j0, h) = gcm_start(aes, nonce);
    let tag = gcm_tag(aes, j0, h, data);
    let mut counter = j0;
    inc32(&mut counter);
    ctr_with(aes, counter, data, inc32);
    tag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn aes_256_is_that_of_fips_197() {
        // FIPS-197 C.3
        let key: [u8; 32] = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
            .try_into()
            .unwrap();
        let mut block: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        Aes256::new(&key).encrypt(&mut block);
        assert_eq!(block.to_vec(), hex("8ea2b7ca516745bfeafc49904b496089"));
    }

    #[test]
    fn ctr_is_that_of_sp_800_38a() {
        // SP 800-38A F.5.5, CTR-AES256.Encrypt
        let key: [u8; 32] = hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4")
            .try_into()
            .unwrap();
        let counter: [u8; 16] = hex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff").try_into().unwrap();
        let mut data = hex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        ctr_full(&Aes256::new(&key), counter, &mut data);
        assert_eq!(
            data,
            hex("601ec313775789a5b7a7f504bbf3d228f443e3ca4d62b59aca84e990cacaf5c5")
        );
    }

    #[test]
    fn gcm_is_that_of_the_gcm_paper_for_both_nonce_lengths() {
        // test cases 15 (a 12-byte nonce) and 18 (a 60-byte one) of McGrew and Viega, without additional data
        let key: [u8; 32] = hex("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308")
            .try_into()
            .unwrap();
        let plaintext = hex(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525\
             b16aedf5aa0de657ba637b391aafd255",
        );
        let encryption = FileEncryption::new(key, Cipher::Gcm, Version::V1);
        let nonce = hex("cafebabefacedbaddecaf888");
        let mut data = plaintext.clone();
        let tag = encryption.seal(Cipher::Gcm, &nonce, &mut data);
        assert_eq!(
            data,
            hex(
                "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa8cb08e48590dbb3da7b08b1056828838\
                 c5f61e6393ba7a0abcc9f662898015ad"
            )
        );
        assert_eq!(tag.to_vec(), hex("b094dac5d93471bdec1a502270e3cc6c"));
        // the same through the GCM of any nonce length
        let mut again = plaintext.clone();
        assert_eq!(gcm_seal(&encryption.aes, &nonce, &mut again), tag);
        assert_eq!(again, data);
        encryption.open(Cipher::Gcm, &nonce, &mut data, &tag).unwrap();
        assert_eq!(data, plaintext);

        // (case 18 has additional data, which changes its tag but not its ciphertext)
        let long_nonce = hex(
            "9313225df88406e555909c5aff5269aa6a7a9538534f7da1e4c303d2a318a728c3c0c95156809539fcf0e2429a6b5254\
             16aedbf5a0de6a57a637b39b",
        );
        let mut data = plaintext[..60].to_vec();
        let tag = gcm_seal(&encryption.aes, &long_nonce, &mut data);
        assert_eq!(
            data,
            hex(
                "5a8def2f0c9e53f1f75d7853659e2a20eeb2b22aafde6419a058ab4f6f746bf40fc0c3b780f244452da3ebf1c5d82cde\
                 a2418997200ef82e44ae7e3f"
            )
        );
        let mut opened = data.clone();
        assert_eq!(gcm_seal_inverse(&encryption.aes, &long_nonce, &mut opened), tag);
        assert_eq!(opened, plaintext[..60]);
    }

    #[test]
    fn blocks_and_log_entries_come_back_and_a_changed_byte_is_found() {
        for (cipher, version) in [
            (Cipher::Gcm, Version::V1),
            (Cipher::Ctr, Version::V1),
            (Cipher::Gcm, Version::V0),
            (Cipher::Ctr, Version::V0),
        ] {
            let encryption = FileEncryption::new(derive_key(b"secret", &[7; 16]), cipher, version);
            let mut block = vec![0u8; 4096];
            for (i, byte) in block.iter_mut().enumerate().skip(32) {
                *byte = i as u8;
            }
            let plain = block.clone();
            encryption.encrypt_block(&mut block).unwrap();
            assert_ne!(block[40..], plain[40..]);
            let mut opened = block.clone();
            encryption.decrypt_block(&mut opened).unwrap();
            assert_eq!(opened[32..], plain[32..]);
            if cipher == Cipher::Gcm {
                block[100] ^= 1;
                assert!(encryption.decrypt_block(&mut block).is_err());
            }

            let framed = encryption.encrypt_log_entry(42, b"an entry").unwrap();
            assert_eq!(framed.len(), encryption.log_entry_overhead() + 8);
            assert_eq!(
                encryption.decrypt_log_entry(&framed).unwrap(),
                (42, b"an entry".to_vec())
            );

            let (canary, nonce, tag) = encryption.canary().unwrap();
            assert!(encryption.check_canary(&canary, &nonce, &tag));
            let other = FileEncryption::new(derive_key(b"other", &[7; 16]), cipher, version);
            assert!(!other.check_canary(&canary, &nonce, &tag));
        }
    }

    #[test]
    fn the_canary_and_headers_of_files_duckdb_1_4_encrypted_open() {
        for (name, expected) in [("gcm", Cipher::Gcm), ("ctr", Cipher::Ctr)] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                "../../../duckdb/data/attach_test/encrypted_{name}_key=abcde.db"
            ));
            // (the files are in the DuckDB checkout next to this one)
            if let Ok(file) = std::fs::read(&path) {
                check_duckdb_file(&file, expected);
            }
        }
    }

    fn check_duckdb_file(file: &[u8], expected: Cipher) {
        let header = &file[8..];
        let metadata = &header[108..116];
        let (cipher, version) = (
            Cipher::from_number(metadata[2]).unwrap(),
            Version::from_number(metadata[3]).unwrap(),
        );
        assert_eq!((cipher, version), (expected, Version::V0));
        let identifier = &header[116..132];
        let canary: [u8; 8] = header[132..140].try_into().unwrap();
        let nonce: [u8; NONCE_LEN] = header[140..152].try_into().unwrap();
        let tag: [u8; TAG_LEN] = header[152..168].try_into().unwrap();
        let encryption = FileEncryption::new(derive_key(b"abcde", identifier), cipher, version);
        assert!(encryption.check_canary(&canary, &nonce, &tag));
        let wrong = FileEncryption::new(derive_key(b"abcdf", identifier), cipher, version);
        assert!(!wrong.check_canary(&canary, &nonce, &tag));
        for slot in [1, 2] {
            let mut block = file[4096 * slot..4096 * (slot + 1)].to_vec();
            encryption.decrypt_block(&mut block).unwrap();
            let stored = u64::from_le_bytes(block[32..40].try_into().unwrap());
            assert_eq!(
                stored,
                crate::storage::checksum::checksum(&block[40..]),
                "header {slot}"
            );
        }
    }

    #[test]
    fn a_key_that_is_base64_is_decoded() {
        let salt = [1; 16];
        let sha = |key: &[u8]| -> [u8; KEY_LENGTH] {
            let mut hasher = sha2::Sha256::new();
            hasher.update(salt);
            hasher.update(key);
            hasher.finalize().into()
        };
        assert_eq!(derive_key(b"YWJjZA==", &salt), sha(b"abcd"));
        // not base64 (its length is no multiple of 4): the key as it is
        assert_eq!(derive_key(b"abcde", &salt), sha(b"abcde"));
    }
}
