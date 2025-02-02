mod reply;
mod request;

pub use self::reply::{AuthenticationReply, KerberosReply, PreauthReply, TicketGrantReply};
pub use self::request::{AuthenticationRequest, KerberosRequest, TicketGrantRequest};

use crate::asn1::{
    constants::{encryption_types::EncryptionType, pa_data_types::PaDataType},
    enc_kdc_rep_part::EncKdcRepPart,
    encrypted_data::EncryptedData as KdcEncryptedData,
    encryption_key::EncryptionKey as KdcEncryptionKey,
    etype_info2::ETypeInfo2 as KdcETypeInfo2,
    kerberos_string::KerberosString,
    pa_data::PaData,
    pa_enc_ts_enc::PaEncTsEnc,
    principal_name::PrincipalName,
    realm::Realm,
    tagged_enc_kdc_rep_part::TaggedEncKdcRepPart,
    tagged_ticket::TaggedTicket as Asn1Ticket,
    ticket_flags::TicketFlags,
    Ia5String, OctetString,
};
use crate::constants::{AES_256_KEY_LEN, PKBDF2_SHA1_ITER, RFC_PKBDF2_SHA1_ITER};
use crate::crypto::{
    decrypt_aes256_cts_hmac_sha1_96, derive_key_aes256_cts_hmac_sha1_96,
    encrypt_aes256_cts_hmac_sha1_96,
};
use crate::error::KrbError;
use der::{flagset::FlagSet, Decode, Encode};
use rand::{thread_rng, Rng};

use std::cmp::Ordering;
use std::fmt;
use std::time::{Duration, SystemTime};
use tracing::trace;

// Zeroize blocked on https://github.com/RustCrypto/block-ciphers/issues/426
// use zeroize::Zeroizing;

#[derive(Debug, Default)]
pub struct Preauth {
    enc_timestamp: Option<EncryptedData>,
    pa_fx_cookie: Option<Vec<u8>>,
}

pub enum DerivedKey {
    Aes256CtsHmacSha196 {
        k: [u8; AES_256_KEY_LEN],
        i: u32,
        s: String,
    },
}

impl DerivedKey {
    pub fn new_aes256_cts_hmac_sha1_96(passphrase: &str, salt: &str) -> Result<Self, KrbError> {
        // let iter_count = PKBDF2_SHA1_ITER;
        let iter_count = RFC_PKBDF2_SHA1_ITER;

        derive_key_aes256_cts_hmac_sha1_96(passphrase.as_bytes(), salt.as_bytes(), iter_count).map(
            |k| DerivedKey::Aes256CtsHmacSha196 {
                k,
                i: iter_count,
                s: salt.to_string(),
            },
        )
    }

    // Used to derive a key for the user. We have to do this to get the correct
    // etype from the enc data as pa_data may have many etype_info2 and the spec
    // doesn't call it an error to have multiple ... yay for confusing poorly
    // structured protocols.
    pub fn from_encrypted_reply(
        encrypted_data: &EncryptedData,
        pa_data_etype_info2: Option<&[EtypeInfo2]>,
        realm: &str,
        username: &str,
        passphrase: &str,
    ) -> Result<Self, KrbError> {
        // If only Krb had put the *parameters* with the encrypted data, like any other
        // sane ecosystem.
        match encrypted_data {
            EncryptedData::Aes256CtsHmacSha196 { .. } => {
                // Find if we have an etype info?

                let maybe_etype_info2 = pa_data_etype_info2
                    .iter()
                    .map(|slice| slice.iter())
                    .flatten()
                    .filter(|etype_info2| {
                        matches!(&etype_info2.etype, EncryptionType::AES256_CTS_HMAC_SHA1_96)
                    })
                    .next();

                let (salt, iter_count) = if let Some(etype_info2) = maybe_etype_info2 {
                    let salt = etype_info2.salt.as_ref().cloned();

                    let iter_count = if let Some(s2kparams) = &etype_info2.s2kparams {
                        if s2kparams.len() != 4 {
                            return Err(KrbError::PreauthInvalidS2KParams);
                        };
                        let mut iter_count = [0u8; 4];
                        iter_count.copy_from_slice(&s2kparams);

                        Some(u32::from_be_bytes(iter_count))
                    } else {
                        None
                    };

                    (salt, iter_count)
                } else {
                    (None, None)
                };

                let salt = salt.unwrap_or_else(|| format!("{}{}", realm, username));

                let iter_count = iter_count.unwrap_or(RFC_PKBDF2_SHA1_ITER);

                derive_key_aes256_cts_hmac_sha1_96(
                    passphrase.as_bytes(),
                    salt.as_bytes(),
                    iter_count,
                )
                .map(|k| DerivedKey::Aes256CtsHmacSha196 {
                    k,
                    i: iter_count,
                    s: salt,
                })
            }
            _ => Err(KrbError::UnsupportedEncryption),
        }
    }

    // This is used in pre-auth timestamp as there is no kvno as I can see?
    pub fn from_etype_info2(
        etype_info2: &EtypeInfo2,
        realm: &str,
        username: &str,
        passphrase: &str,
    ) -> Result<Self, KrbError> {
        let salt = etype_info2
            .salt
            .as_ref()
            .cloned()
            .unwrap_or_else(|| format!("{}{}", realm, username));

        match &etype_info2.etype {
            EncryptionType::AES256_CTS_HMAC_SHA1_96 => {
                // Iter count is from the s2kparams
                let iter_count = if let Some(s2kparams) = &etype_info2.s2kparams {
                    if s2kparams.len() != 4 {
                        return Err(KrbError::PreauthInvalidS2KParams);
                    };
                    let mut iter_count = [0u8; 4];
                    iter_count.copy_from_slice(&s2kparams);

                    u32::from_be_bytes(iter_count)
                } else {
                    // Assume the insecure default rfc value.
                    RFC_PKBDF2_SHA1_ITER
                };

                derive_key_aes256_cts_hmac_sha1_96(
                    passphrase.as_bytes(),
                    salt.as_bytes(),
                    iter_count,
                )
                .map(|k| DerivedKey::Aes256CtsHmacSha196 {
                    k,
                    i: iter_count,
                    s: salt,
                })
            }
            _ => Err(KrbError::UnsupportedEncryption),
        }
    }

    pub fn encrypt_pa_enc_timestamp(
        &self,
        paenctsenc: &PaEncTsEnc,
    ) -> Result<EncryptedData, KrbError> {
        let data = paenctsenc
            .to_der()
            .map_err(|_| KrbError::DerEncodePaEncTsEnc)?;

        // https://www.rfc-editor.org/rfc/rfc4120#section-5.2.7.2
        let key_usage = 1;

        match self {
            DerivedKey::Aes256CtsHmacSha196 { k, .. } => {
                encrypt_aes256_cts_hmac_sha1_96(k, &data, key_usage)
                    .map(|data| EncryptedData::Aes256CtsHmacSha196 { kvno: None, data })
            }
        }
    }
}

impl fmt::Debug for DerivedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("DerivedKey");
        match self {
            DerivedKey::Aes256CtsHmacSha196 { i, s, .. } => builder
                .field("k", &"Aes256HmacSha1")
                .field("i", i)
                .field("s", s),
        }
        .finish()
    }
}

pub enum SessionKey {
    Aes256CtsHmacSha196 { k: [u8; AES_256_KEY_LEN] },
}

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("SessionKey");
        match self {
            SessionKey::Aes256CtsHmacSha196 { .. } => builder.field("k", &"Aes256"),
        }
        .finish()
    }
}

pub enum KdcPrimaryKey {
    Aes256 { k: [u8; AES_256_KEY_LEN] },
}

impl fmt::Debug for KdcPrimaryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("KdcPrimaryKey");
        match self {
            KdcPrimaryKey::Aes256 { .. } => builder.field("k", &"Aes256"),
        }
        .finish()
    }
}

impl TryFrom<&[u8]> for KdcPrimaryKey {
    type Error = KrbError;

    fn try_from(key: &[u8]) -> Result<Self, Self::Error> {
        if key.len() == AES_256_KEY_LEN {
            let mut k = [0u8; AES_256_KEY_LEN];
            k.copy_from_slice(key);
            Ok(KdcPrimaryKey::Aes256 { k })
        } else {
            tracing::error!(key_len = %key.len(), expected = %AES_256_KEY_LEN);
            Err(KrbError::InvalidEncryptionKey)
        }
    }
}

#[derive(Debug)]
pub struct Ticket {
    tkt_vno: i8,
    service: Name,
    enc_part: EncryptedData,
}

// pub struct LastRequest

#[derive(Debug)]
pub struct KdcReplyPart {
    key: SessionKey,
    // Last req shows "last login" and probably isn't important for our needs.
    // last_req: (),
    nonce: u32,
    key_expiration: Option<SystemTime>,
    flags: FlagSet<TicketFlags>,
    auth_time: SystemTime,
    start_time: Option<SystemTime>,
    end_time: SystemTime,
    renew_until: Option<SystemTime>,
    server: Name,
    // Shows the addresses the ticket may be used from. Mostly these are broken
    // by nat, and so aren't used. These are just to display that there are limits
    // to the client, the enforced addrs are in the ticket.
    // client_addresses: Vec<HostAddress>,
}

#[derive(Debug)]
pub enum EncryptedData {
    Aes256CtsHmacSha196 { kvno: Option<u32>, data: Vec<u8> },
}

#[derive(Debug, Default)]
pub struct PreauthData {
    pub(crate) pa_fx_fast: bool,
    pub(crate) enc_timestamp: bool,
    pub(crate) pa_fx_cookie: Option<Vec<u8>>,
    pub(crate) etype_info2: Vec<EtypeInfo2>,
}

#[derive(Debug, Clone)]
pub enum Name {
    Principal {
        name: String,
        realm: String,
    },
    SrvInst {
        service: String,
        realm: String,
    },
    SrvHst {
        service: String,
        host: String,
        realm: String,
    },
    /*
    Uid {
    }
    */
}

#[derive(Debug, Clone)]
pub struct EtypeInfo2 {
    // The type of encryption for enc ts.
    etype: EncryptionType,

    salt: Option<String>,

    // For AES HMAC SHA1:
    //   The number of iterations is specified by the string-to-key parameters
    //   supplied.  The parameter string is four octets indicating an unsigned
    //   number in big-endian order.  This is the number of iterations to be
    //   performed.  If the value is 00 00 00 00, the number of iterations to
    //   be performed is 4,294,967,296 (2**32).  (Thus the minimum expressible
    //   iteration count is 1.)
    s2kparams: Option<Vec<u8>>,
}

fn sort_cryptographic_strength(a: &EtypeInfo2, b: &EtypeInfo2) -> Ordering {
    /*
    if a.etype == EncryptionType::AES256_CTS_HMAC_SHA384_192 {
        Ordering::Greater
    } else if b.etype == EncryptionType::AES256_CTS_HMAC_SHA384_192 {
        Ordering::Less
    } else if a.etype == EncryptionType::AES128_CTS_HMAC_SHA256_128 {
        Ordering::Greater
    } else if b.etype == EncryptionType::AES128_CTS_HMAC_SHA256_128 {
        Ordering::Less
    } else
    */
    if a.etype == EncryptionType::AES256_CTS_HMAC_SHA1_96 {
        Ordering::Greater
    } else if b.etype == EncryptionType::AES256_CTS_HMAC_SHA1_96 {
        Ordering::Less
    } else if a.etype == EncryptionType::AES128_CTS_HMAC_SHA1_96 {
        Ordering::Greater
    } else if b.etype == EncryptionType::AES128_CTS_HMAC_SHA1_96 {
        Ordering::Less
    } else {
        // Everything else is trash.
        Ordering::Equal
    }
}

impl TryFrom<Vec<PaData>> for PreauthData {
    type Error = KrbError;

    fn try_from(pavec: Vec<PaData>) -> Result<Self, Self::Error> {
        // Per https://www.rfc-editor.org/rfc/rfc4120#section-7.5.2
        // Build up the set of PaRep data
        let mut pa_fx_fast = false;
        let mut enc_timestamp = false;
        let mut pa_fx_cookie = None;
        let mut etype_info2 = Vec::with_capacity(0);

        for PaData {
            padata_type,
            padata_value,
        } in pavec
        {
            let Ok(padt) = padata_type.try_into() else {
                // padatatype that we don't support
                continue;
            };

            match padt {
                PaDataType::PaEncTimestamp => enc_timestamp = true,
                PaDataType::PaEtypeInfo2 => {
                    // this is a sequence of etypeinfo2
                    let einfo2_sequence = KdcETypeInfo2::from_der(padata_value.as_bytes())
                        .map_err(|_| KrbError::DerDecodeEtypeInfo2)?;

                    for einfo2 in einfo2_sequence {
                        let Ok(etype) = EncryptionType::try_from(einfo2.etype) else {
                            // Invalid etype or we don't support it.
                            continue;
                        };

                        // Only proceed with what we support.
                        match etype {
                            EncryptionType::AES256_CTS_HMAC_SHA1_96 => {}
                            _ => continue,
                        };

                        // I think at this point we should ignore any etypes we don't support.
                        let salt = einfo2.salt.map(|s| s.into());
                        let s2kparams = einfo2.s2kparams.map(|v| v.as_bytes().to_vec());

                        etype_info2.push(EtypeInfo2 {
                            etype,
                            salt,
                            s2kparams,
                        });
                    }
                }
                PaDataType::PaFxFast => pa_fx_fast = true,
                PaDataType::PaFxCookie => pa_fx_cookie = Some(padata_value.as_bytes().to_vec()),
                _ => {
                    // Ignore unsupported pa data types.
                }
            };
        }

        // Sort the etype_info by cryptographic strength.
        etype_info2.sort_unstable_by(sort_cryptographic_strength);

        Ok(PreauthData {
            pa_fx_fast,
            pa_fx_cookie,
            enc_timestamp,
            etype_info2,
        })
    }
}

impl TryFrom<Vec<PaData>> for Preauth {
    type Error = KrbError;

    fn try_from(pavec: Vec<PaData>) -> Result<Self, Self::Error> {
        let mut preauth = Preauth::default();

        for PaData {
            padata_type,
            padata_value,
        } in pavec
        {
            let Ok(padt) = padata_type.try_into() else {
                // padatatype that we don't support
                continue;
            };

            match padt {
                PaDataType::PaEncTimestamp => {
                    let enc_timestamp = KdcEncryptedData::from_der(padata_value.as_bytes())
                        .map_err(|_| KrbError::DerDecodePaData)
                        .and_then(EncryptedData::try_from)?;
                    preauth.enc_timestamp = Some(enc_timestamp);
                }
                PaDataType::PaFxCookie => {
                    preauth.pa_fx_cookie = Some(padata_value.as_bytes().to_vec())
                }
                _ => {
                    // Ignore unsupported pa data types.
                }
            };
        }

        Ok(preauth)
    }
}

impl EncryptedData {
    fn decrypt_data(&self, base_key: &DerivedKey, key_usage: i32) -> Result<Vec<u8>, KrbError> {
        match (self, base_key) {
            (
                EncryptedData::Aes256CtsHmacSha196 { kvno: _, data },
                DerivedKey::Aes256CtsHmacSha196 { k, .. },
            ) => decrypt_aes256_cts_hmac_sha1_96(&k, &data, key_usage),
        }
    }

    pub fn decrypt_enc_kdc_rep(&self, base_key: &DerivedKey) -> Result<KdcReplyPart, KrbError> {
        // RFC 4120 The key usage value for encrypting this field is 3 in an AS-REP
        // message, using the client's long-term key or another key selected
        // via pre-authentication mechanisms.
        let data = self.decrypt_data(base_key, 3)?;

        let tagged_kdc_enc_part = TaggedEncKdcRepPart::from_der(&data).map_err(|e| {
            println!("{:#?}", e);
            KrbError::DerDecodeEncKdcRepPart
        })?;

        // RFC states we should relax the tag check on these.

        let kdc_enc_part = match tagged_kdc_enc_part {
            TaggedEncKdcRepPart::EncTgsRepPart(part) | TaggedEncKdcRepPart::EncAsRepPart(part) => {
                part
            }
        };

        KdcReplyPart::try_from(kdc_enc_part)
    }

    pub fn decrypt_pa_enc_timestamp(&self, base_key: &DerivedKey) -> Result<SystemTime, KrbError> {
        // https://www.rfc-editor.org/rfc/rfc4120#section-5.2.7.2
        let data = self.decrypt_data(base_key, 1)?;

        let paenctsenc = PaEncTsEnc::from_der(&data).map_err(|_| KrbError::DerDecodePaEncTsEnc)?;

        trace!(?paenctsenc);

        let stime = paenctsenc.patimestamp.to_system_time();
        let usecs = paenctsenc
            .pausec
            .map(|s| Duration::from_micros(s as u64))
            .unwrap_or_default();

        let stime = stime + usecs;

        Ok(stime)
    }
}

impl TryFrom<KdcEncryptedData> for EncryptedData {
    type Error = KrbError;

    fn try_from(enc_data: KdcEncryptedData) -> Result<Self, Self::Error> {
        let etype: EncryptionType = EncryptionType::try_from(enc_data.etype)
            .map_err(|_| KrbError::UnsupportedEncryption)?;
        match etype {
            EncryptionType::AES256_CTS_HMAC_SHA1_96 => {
                // todo! there is some way to get a number of rounds here
                // but I can't obviously see it?
                let kvno = enc_data.kvno;
                let data = enc_data.cipher.into_bytes();
                Ok(EncryptedData::Aes256CtsHmacSha196 { kvno, data })
            }
            _ => Err(KrbError::UnsupportedEncryption),
        }
    }
}

impl TryInto<KdcEncryptedData> for EncryptedData {
    type Error = KrbError;

    fn try_into(self) -> Result<KdcEncryptedData, KrbError> {
        match self {
            EncryptedData::Aes256CtsHmacSha196 { kvno, data } => Ok(KdcEncryptedData {
                etype: EncryptionType::AES256_CTS_HMAC_SHA1_96 as i32,
                kvno,
                cipher: OctetString::new(data).map_err(|e| {
                    println!("{:#?}", e);
                    KrbError::UnsupportedEncryption // TODO
                })?,
            }),
        }
    }
}

impl TryFrom<Asn1Ticket> for Ticket {
    type Error = KrbError;

    fn try_from(tkt: Asn1Ticket) -> Result<Self, Self::Error> {
        let Asn1Ticket(tkt) = tkt;

        let service = Name::try_from((tkt.sname, tkt.realm))?;
        let enc_part = EncryptedData::try_from(tkt.enc_part)?;
        let tkt_vno = tkt.tkt_vno;

        Ok(Ticket {
            tkt_vno,
            service,
            enc_part,
        })
    }
}

impl TryInto<Asn1Ticket> for Ticket {
    type Error = KrbError;

    fn try_into(self) -> Result<Asn1Ticket, KrbError> {
        let t = crate::asn1::tagged_ticket::Ticket {
            tkt_vno: self.tkt_vno,
            realm: (&self.service).try_into()?,
            sname: (&self.service).try_into()?,
            enc_part: self.enc_part.try_into()?,
        };
        Ok(Asn1Ticket::new(t))
    }
}

impl TryFrom<EncKdcRepPart> for KdcReplyPart {
    type Error = KrbError;

    fn try_from(enc_kdc_rep_part: EncKdcRepPart) -> Result<Self, Self::Error> {
        trace!(?enc_kdc_rep_part);

        let key = SessionKey::try_from(enc_kdc_rep_part.key)?;
        let server = Name::try_from((enc_kdc_rep_part.server_name, enc_kdc_rep_part.server_realm))?;

        let nonce = enc_kdc_rep_part.nonce;
        // let flags = enc_kdc_rep_part.flags.bits();
        let flags = enc_kdc_rep_part.flags;

        let key_expiration = enc_kdc_rep_part.key_expiration.map(|t| t.to_system_time());
        let start_time = enc_kdc_rep_part.start_time.map(|t| t.to_system_time());
        let renew_until = enc_kdc_rep_part.renew_till.map(|t| t.to_system_time());
        let auth_time = enc_kdc_rep_part.auth_time.to_system_time();
        let end_time = enc_kdc_rep_part.end_time.to_system_time();

        Ok(KdcReplyPart {
            key,
            nonce,
            key_expiration,
            flags,
            auth_time,
            start_time,
            end_time,
            renew_until,
            server,
        })
    }
}

impl TryFrom<KdcEncryptionKey> for SessionKey {
    type Error = KrbError;

    fn try_from(kdc_key: KdcEncryptionKey) -> Result<Self, Self::Error> {
        let key_type = EncryptionType::try_from(kdc_key.key_type)
            .map_err(|_| KrbError::UnsupportedEncryption)?;
        match key_type {
            EncryptionType::AES256_CTS_HMAC_SHA1_96 => {
                if kdc_key.key_value.as_bytes().len() == AES_256_KEY_LEN {
                    let mut k = [0u8; AES_256_KEY_LEN];
                    k.copy_from_slice(kdc_key.key_value.as_bytes());
                    Ok(SessionKey::Aes256CtsHmacSha196 { k })
                } else {
                    Err(KrbError::InvalidEncryptionKey)
                }
            }
            _ => Err(KrbError::UnsupportedEncryption),
        }
    }
}

impl Name {
    pub fn principal(name: &str, realm: &str) -> Self {
        Self::Principal {
            name: name.to_string(),
            realm: realm.to_string(),
        }
    }

    pub fn service_krbtgt(realm: &str) -> Self {
        Self::SrvInst {
            service: "krbtgt".to_string(),
            realm: realm.to_string(),
        }
    }

    pub fn is_service_krbtgt(&self, check_realm: &str) -> bool {
        match self {
            Self::SrvInst { service, realm } => service == "krbtgt" && check_realm == realm,
            _ => false,
        }
    }

    /// If the name is a PRINCIPAL then return it's name and realm compontents. If
    /// not, then an error is returned.
    pub fn principal_name(&self) -> Result<(&str, &str), KrbError> {
        match self {
            Name::Principal { name, realm } => Ok((name.as_str(), realm.as_str())),
            _ => Err(KrbError::NameNotPrincipal),
        }
    }
}

impl TryInto<Realm> for &Name {
    type Error = KrbError;

    fn try_into(self) -> Result<Realm, KrbError> {
        match self {
            Name::Principal { name, realm } => {
                let realm = KerberosString(Ia5String::new(realm).unwrap());
                Ok(realm)
            }
            Name::SrvInst { service, realm } => {
                let realm = KerberosString(Ia5String::new(realm).unwrap());
                Ok(realm)
            }
            Name::SrvHst {
                service,
                host,
                realm,
            } => {
                let realm = KerberosString(Ia5String::new(realm).unwrap());
                Ok(realm)
            }
        }
    }
}

impl TryInto<PrincipalName> for &Name {
    type Error = KrbError;

    fn try_into(self) -> Result<PrincipalName, KrbError> {
        match self {
            Name::Principal { name, realm } => {
                let name_string = vec![
                    KerberosString(Ia5String::new(name).unwrap()),
                    KerberosString(Ia5String::new(realm).unwrap()),
                ];

                Ok(PrincipalName {
                    name_type: 1,
                    name_string,
                })
            }
            Name::SrvInst { service, realm } => {
                let name_string = vec![
                    KerberosString(Ia5String::new(service).unwrap()),
                    KerberosString(Ia5String::new(realm).unwrap()),
                ];

                Ok(PrincipalName {
                    name_type: 2,
                    name_string,
                })
            }
            Name::SrvHst {
                service,
                host,
                realm,
            } => {
                let name_string = vec![
                    KerberosString(Ia5String::new(service).unwrap()),
                    KerberosString(Ia5String::new(host).unwrap()),
                    KerberosString(Ia5String::new(realm).unwrap()),
                ];

                Ok(PrincipalName {
                    name_type: 3,
                    name_string,
                })
            }
        }
    }
}

impl TryInto<(PrincipalName, Realm)> for &Name {
    type Error = KrbError;

    fn try_into(self) -> Result<(PrincipalName, Realm), KrbError> {
        match self {
            Name::Principal { name, realm } => {
                let name_string = vec![KerberosString(Ia5String::new(&name).unwrap())];
                let realm = KerberosString(Ia5String::new(realm).unwrap());

                Ok((
                    PrincipalName {
                        name_type: 1,
                        name_string,
                    },
                    realm,
                ))
            }
            Name::SrvInst { service, realm } => {
                let name_string = vec![KerberosString(Ia5String::new(&service).unwrap())];
                let realm = KerberosString(Ia5String::new(realm).unwrap());

                Ok((
                    PrincipalName {
                        name_type: 2,
                        name_string,
                    },
                    realm,
                ))
            }
            Name::SrvHst {
                service,
                host,
                realm,
            } => {
                let name_string = vec![
                    KerberosString(Ia5String::new(&service).unwrap()),
                    KerberosString(Ia5String::new(&host).unwrap()),
                ];
                let realm = KerberosString(Ia5String::new(realm).unwrap());

                Ok((
                    PrincipalName {
                        name_type: 3,
                        name_string,
                    },
                    realm,
                ))
            }
        }
    }
}

impl TryFrom<PrincipalName> for Name {
    type Error = KrbError;

    fn try_from(princ: PrincipalName) -> Result<Self, Self::Error> {
        let PrincipalName {
            name_type,
            name_string,
        } = princ;
        match name_type {
            1 => {
                let name = name_string.get(0).unwrap().into();
                let realm = name_string.get(1).unwrap().into();
                Ok(Name::Principal { name, realm })
            }
            2 => {
                let service = name_string.get(0).unwrap().into();
                let realm = name_string.get(1).unwrap().into();
                Ok(Name::SrvInst { service, realm })
            }
            3 => {
                let service = name_string.get(0).unwrap().into();
                let host = name_string.get(1).unwrap().into();
                let realm = name_string.get(2).unwrap().into();
                Ok(Name::SrvHst {
                    service,
                    host,
                    realm,
                })
            }
            _ => todo!(),
        }
    }
}

impl TryFrom<(PrincipalName, Realm)> for Name {
    type Error = KrbError;

    fn try_from((princ, realm): (PrincipalName, Realm)) -> Result<Self, Self::Error> {
        let PrincipalName {
            name_type,
            name_string,
        } = princ;

        let realm = realm.into();

        match name_type {
            1 => {
                let name = name_string.get(0).unwrap().into();
                Ok(Name::Principal { name, realm })
            }
            2 => {
                let service = name_string.get(0).unwrap().into();
                Ok(Name::SrvInst { service, realm })
            }
            3 => {
                let service = name_string.get(0).unwrap().into();
                let host = name_string.get(1).unwrap().into();
                Ok(Name::SrvHst {
                    service,
                    host,
                    realm,
                })
            }
            _ => todo!(),
        }
    }
}

impl Preauth {
    pub fn enc_timestamp(&self) -> Option<&EncryptedData> {
        self.enc_timestamp.as_ref()
    }
}
