use crate::group::GroupInfo;
use crate::keypackage::{KeyPackage, OwnedKeyPackage};
use crate::message::*;
use crate::utils::{encode_vec_u32, encode_vec_u8_u8, read_vec_u32, read_vec_u8_u8};
use aead::{Aead, NewAead};
use hkdf::{Hkdf, InvalidLength};
use hpke::{
    aead::{AeadTag, AesGcm128},
    kex::{Marshallable, Unmarshallable},
    EncappedKey,
};
use rustls::internal::msgs::codec::{Codec, Reader};
use secrecy::{ExposeSecret, SecretVec};
use sha2::digest::generic_array::{self, ArrayLength, GenericArray};
use sha2::digest::{BlockInput, FixedOutput, Input, Reset};
use sha2::{Digest, Sha256};

#[allow(non_camel_case_types)]
#[repr(u16)]
#[derive(Clone)]
pub enum CipherSuite {
    MLS10_128_DHKEMP256_AES128GCM_SHA256_P256 = 2,
}

/// spec: draft-ietf-mls-protocol.md#key-schedule
#[derive(Debug)]
struct HKDFLabel {
    // 0..255 -- hash of group context
    pub group_context: Vec<u8>,
    pub length: u16,
    // 7..255 -- prefixed with "mls10 "
    pub label: Vec<u8>,
    // 0..2^32-1
    pub context: Vec<u8>,
}

impl Codec for HKDFLabel {
    fn encode(&self, bytes: &mut Vec<u8>) {
        encode_vec_u8_u8(bytes, &self.group_context);
        self.length.encode(bytes);
        encode_vec_u8_u8(bytes, &self.label);
        encode_vec_u32(bytes, &self.context);
    }

    fn read(r: &mut Reader) -> Option<Self> {
        let group_context = read_vec_u8_u8(r)?;
        let length = u16::read(r)?;
        let label = read_vec_u8_u8(r)?;
        let context: Vec<u8> = read_vec_u32(r)?;
        Some(Self {
            group_context,
            length,
            label,
            context,
        })
    }
}

/// Additional methods for Kdf
pub trait HkdfExt<D>
where
    D: Input + BlockInput + FixedOutput + Reset + Default + Clone,
    D::BlockSize: ArrayLength<u8>,
    D::OutputSize: ArrayLength<u8>,
{
    fn expand_label(
        &self,
        group_context_hash: Vec<u8>,
        label: &str,
        context: &str,
        length: u16,
    ) -> Result<Vec<u8>, hkdf::InvalidLength>;
    fn derive_secret(
        &self,
        group_context_hash: Vec<u8>,
        label: &str,
    ) -> Result<Vec<u8>, hkdf::InvalidLength>;
}

impl<D> HkdfExt<D> for Hkdf<D>
where
    D: Input + BlockInput + FixedOutput + Reset + Default + Clone,
{
    fn expand_label(
        &self,
        group_context_hash: Vec<u8>,
        label: &str,
        context: &str,
        length: u16,
    ) -> Result<Vec<u8>, InvalidLength> {
        let full_label = "mls10 ".to_owned() + label;
        let labeled_payload = HKDFLabel {
            group_context: group_context_hash,
            length,
            label: full_label.into_bytes().to_vec(),
            context: context.as_bytes().to_vec(),
        }
        .get_encoding();
        let mut okm = vec![0u8; length as usize];
        self.expand(&labeled_payload, &mut okm)?;
        Ok(okm)
    }

    fn derive_secret(
        &self,
        group_context_hash: Vec<u8>,
        label: &str,
    ) -> Result<Vec<u8>, InvalidLength> {
        use generic_array::typenum::Unsigned;
        self.expand_label(group_context_hash, label, "", D::OutputSize::to_u16())
    }
}

impl CipherSuite {
    pub fn aead_key_len(&self) -> usize {
        match self {
            CipherSuite::MLS10_128_DHKEMP256_AES128GCM_SHA256_P256 => 16,
        }
    }

    pub fn aead_nonce_len(&self) -> usize {
        match self {
            CipherSuite::MLS10_128_DHKEMP256_AES128GCM_SHA256_P256 => 12,
        }
    }

    /// TODO: use generic array?
    /// kem_output, context = SetupBaseS(node_public_key, "")
    /// ciphertext = context.Seal(group_context, group_secret)
    pub fn seal_group_secret(
        &self,
        group_secret: GroupSecret,
        // FIXME: only for updates/with path secrets?
        // group_context: &GroupContext,
        kp: &KeyPackage,
    ) -> EncryptedGroupSecrets {
        match self {
            CipherSuite::MLS10_128_DHKEMP256_AES128GCM_SHA256_P256 => {
                let mut csprng = rand::thread_rng();
                let recip_pk = &kp.payload.init_key;
                let key_package_hash = self.hash(&kp.get_encoding());
                let (kem_output, mut context) = hpke::setup_sender::<
                    AesGcm128,
                    hpke::kdf::HkdfSha256,
                    hpke::kem::DhP256HkdfSha256,
                    _,
                >(
                    &hpke::OpModeS::Base,
                    recip_pk.kex_pubkey(),
                    b"",
                    &mut csprng,
                )
                .expect("setup sender");
                let mut output = group_secret.get_encoding();
                let tag = context
                    .seal(&mut output, &[]) // FIXME ?: &group_context.get_encoding())
                    .expect("encryption failed");
                output.extend_from_slice(&tag.marshal());

                EncryptedGroupSecrets {
                    encrypted_group_secrets: HPKECiphertext {
                        kem_output: kem_output.marshal().to_vec(),
                        ciphertext: output,
                    },
                    key_package_hash,
                }
            }
        }
    }

    /// TODO: use generic array?
    pub fn open_group_secret(
        &self,
        encrypted_group_secret: &EncryptedGroupSecrets,
        // FIXME: group_context: &GroupContext,
        open_kp: &OwnedKeyPackage,
    ) -> GroupSecret {
        match self {
            CipherSuite::MLS10_128_DHKEMP256_AES128GCM_SHA256_P256 => {
                // FIXME: errors instead of panicking
                let recip_secret = open_kp.init_private_key.kex_secret();
                let encapped_key =
                    EncappedKey::<<hpke::kem::DhP256HkdfSha256 as hpke::kem::Kem>::Kex>::unmarshal(
                        &encrypted_group_secret.encrypted_group_secrets.kem_output,
                    )
                    .expect("valid encapped key");
                let payload_len = encrypted_group_secret
                    .encrypted_group_secrets
                    .ciphertext
                    .len();
                let mut payload = encrypted_group_secret.encrypted_group_secrets.ciphertext
                    [0..payload_len - 16]
                    .to_vec();
                let tag_bytes = &encrypted_group_secret.encrypted_group_secrets.ciphertext
                    [payload_len - 16..payload_len];

                let tag = AeadTag::<AesGcm128>::unmarshal(tag_bytes).expect("valid tag");

                let mut receiver_ctx =
                    hpke::setup_receiver::<
                        AesGcm128,
                        hpke::kdf::HkdfSha256,
                        hpke::kem::DhP256HkdfSha256,
                    >(&hpke::OpModeR::Base, &recip_secret, &encapped_key, b"")
                    .expect("setup receiver");

                receiver_ctx
                    .open(&mut payload, &[], &tag) // FIXME: group context?
                    .expect("decryption failed");
                GroupSecret::read_bytes(&payload).expect("decoding group secret")
            }
        }
    }

    /// TODO: use generic array?
    pub fn open_group_info(
        &self,
        encrypted_group_info: &[u8],
        welcome_key: SecretVec<u8>,
        welcome_nonce: Vec<u8>,
    ) -> GroupInfo {
        match self {
            CipherSuite::MLS10_128_DHKEMP256_AES128GCM_SHA256_P256 => {
                let aead = <AesGcm128 as hpke::aead::Aead>::AeadImpl::new(
                    GenericArray::clone_from_slice(welcome_key.expose_secret()),
                );
                let nonce = GenericArray::from_slice(&welcome_nonce);
                GroupInfo::read_bytes(
                    &aead
                        .decrypt(nonce, encrypted_group_info)
                        .expect("decryption failure!"),
                )
                .expect("decoding failure")
            }
        }
    }

    /// TODO: use generic array?
    pub fn encrypt_group_info(
        &self,
        group_info: &GroupInfo,
        welcome_key: SecretVec<u8>,
        welcome_nonce: Vec<u8>,
    ) -> Vec<u8> {
        match self {
            CipherSuite::MLS10_128_DHKEMP256_AES128GCM_SHA256_P256 => {
                let aead = <AesGcm128 as hpke::aead::Aead>::AeadImpl::new(
                    GenericArray::clone_from_slice(welcome_key.expose_secret()),
                );
                let nonce = GenericArray::from_slice(&welcome_nonce);
                aead.encrypt(nonce, group_info.get_encoding().as_ref())
                    .expect("encryption failure!")
            }
        }
    }

    /// TODO: use generic array?
    pub fn hash(&self, data: &[u8]) -> Vec<u8> {
        match self {
            CipherSuite::MLS10_128_DHKEMP256_AES128GCM_SHA256_P256 => Sha256::digest(data).to_vec(),
        }
    }

    pub fn hash_len(&self) -> usize {
        match self {
            CipherSuite::MLS10_128_DHKEMP256_AES128GCM_SHA256_P256 => 32,
        }
    }
}
