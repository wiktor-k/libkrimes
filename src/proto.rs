use crate::asn1::{
    constants::{
        encryption_types::EncryptionType, errors::KrbErrorCode, message_types::KrbMessageType,
        pa_data_types::PaDataType,
    },
    encrypted_data::EncryptedData as KdcEncryptedData,
    etype_info2::ETypeInfo2 as KdcETypeInfo2,
    kdc_rep::KdcRep,
    kdc_req::KdcReq,
    kdc_req_body::KdcReqBody,
    kerberos_flags::KerberosFlags,
    kerberos_string::KerberosString,
    kerberos_time::KerberosTime,
    krb_error::MethodData,
    krb_kdc_req::KrbKdcReq,
    pa_data::PaData,
    principal_name::PrincipalName,
    Ia5String,
};
use crate::constants::AES_256_KEY_LEN;
use crate::crypto::{decrypt_aes256_cts_hmac_sha1_96, derive_key_aes256_cts_hmac_sha1_96};
use crate::error::KrbError;
use der::{flagset::FlagSet, Decode, Encode, Tag, TagNumber};
use rand::{thread_rng, Rng};

use std::time::SystemTime;
use tracing::trace;

#[derive(Debug)]
pub enum KerberosRequest {
    AsReq(KerberosAsReq),
}

#[derive(Debug)]
pub enum KerberosResponse {
    AsRep(KerberosAsRep),
    TgsRep(KerberosTgsRep),
    // This is it's own valid state, not an error, so we return it
    // as a valid response instead.
    PaRep(KerberosPaRep),
    ErrRep(KrbErrorCode),
}

#[derive(Debug)]
pub struct KerberosAsReqBuilder {
    client_name: String,
    service_name: String,
    from: Option<SystemTime>,
    until: SystemTime,
    renew: Option<SystemTime>,
}

#[derive(Debug)]
pub struct KerberosAsReq {
    nonce: u32,
    client_name: String,
    service_name: String,
    from: Option<SystemTime>,
    until: SystemTime,
    renew: Option<SystemTime>,
}

pub enum BaseKey {
    Aes256 {
        // Todo zeroizing.
        k: [u8; AES_256_KEY_LEN],
    },
}

#[derive(Debug)]
pub enum EncryptedData {
    Aes256CtsHmacSha196 { kvno: Option<u32>, data: Vec<u8> },
}

#[derive(Debug)]
pub struct KerberosAsRep {
    pub(crate) client_realm: String,
    pub(crate) client_name: String,
    pub(crate) enc_part: EncryptedData,
}

#[derive(Debug)]
pub struct KerberosTgsRep {}

#[derive(Debug)]
pub struct PreAuthData {
    pub(crate) pa_type: u32,
    pub(crate) pa_value: Vec<u8>,
}

#[derive(Debug)]
pub struct KerberosPaRep {
    pub(crate) pa_fx_fast: bool,
    pub(crate) enc_timestamp: bool,
    pub(crate) pa_fx_cookie: Option<Vec<u8>>,
    pub(crate) etype_info2: Vec<EtypeInfo2>,
}

#[derive(Debug)]
pub struct EtypeInfo2 {
    // The type of encryption for enc ts.
    etype: EncryptionType,
    // Should probably be vecu8 ...
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

#[derive(Debug)]
enum KerberosErrRep {
    Err(KrbErrorCode),
    Pa(KerberosPaRep),
}

impl KerberosRequest {
    pub fn build_asreq(
        client_name: String,
        service_name: String,
        from: Option<SystemTime>,
        until: SystemTime,
        renew: Option<SystemTime>,
    ) -> KerberosAsReqBuilder {
        KerberosAsReqBuilder {
            client_name,
            service_name,
            from,
            until,
            renew,
        }
    }

    pub(crate) fn from_der(der: Vec<u8>) -> Result<Self, der::Error> {
        todo!();
    }

    pub(crate) fn to_der(&self) -> Result<Vec<u8>, der::Error> {
        match self {
            KerberosRequest::AsReq(as_req) => KrbKdcReq::to_der(&KrbKdcReq::AsReq(as_req.to_asn())),
        }
    }
}

impl<'a> ::der::Decode<'a> for KerberosResponse {
    fn decode<R: der::Reader<'a>>(decoder: &mut R) -> der::Result<Self> {
        let tag: der::Tag = decoder.decode()?;
        let _len: der::Length = decoder.decode()?;

        match tag {
            Tag::Application {
                constructed: true,
                number: TagNumber::N11,
            } => {
                let kdc_rep: KdcRep = decoder.decode()?;
                //let kdc_rep: KrbKdcRep = KrbKdcRep::AsRep(kdc_rep);
                let as_rep: KerberosAsRep =
                    KerberosAsRep::try_from(kdc_rep).expect("Failed to parse as rep");
                Ok(KerberosResponse::AsRep(as_rep))
            }
            Tag::Application {
                constructed: true,
                number: TagNumber::N13,
            } => {
                let kdc_rep: KdcRep = decoder.decode()?;
                let tgs_rep: KerberosTgsRep =
                    KerberosTgsRep::try_from(kdc_rep).expect("Failed to parse tgs rep");
                Ok(KerberosResponse::TgsRep(tgs_rep))
            }
            Tag::Application {
                constructed: true,
                number: TagNumber::N30,
            } => {
                let kdc_rep: crate::asn1::krb_error::KrbError = decoder.decode()?;
                // Kerberos encodes state in some error resposes, and so we need to disambiguate
                // that here.
                let err_rep: KerberosErrRep =
                    KerberosErrRep::try_from(kdc_rep).expect("Failed to parse err rep");

                Ok(match err_rep {
                    KerberosErrRep::Pa(pa_rep) => KerberosResponse::PaRep(pa_rep),
                    KerberosErrRep::Err(err_code) => KerberosResponse::ErrRep(err_code),
                })
            }
            _ => Err(der::Error::from(der::ErrorKind::TagUnexpected {
                expected: None,
                actual: tag,
            })),
        }
    }
}

impl KerberosAsReqBuilder {
    pub fn build(self) -> KerberosRequest {
        let KerberosAsReqBuilder {
            client_name,
            service_name,
            from,
            until,
            renew,
        } = self;

        let nonce = thread_rng().gen();

        KerberosRequest::AsReq(KerberosAsReq {
            nonce,
            client_name,
            service_name,
            from,
            until,
            renew,
        })
    }
}

impl KerberosAsReq {
    fn to_asn(&self) -> KdcReq {
        KdcReq {
            pvno: 5,
            msg_type: KrbMessageType::KrbAsReq as u8,
            padata: None,
            req_body: KdcReqBody {
                kdc_options: FlagSet::<KerberosFlags>::new(0b0)
                    .expect("Failed to build kdc_options"),
                cname: Some(PrincipalName {
                    // Should be some kind of enum probably?
                    name_type: 1,
                    name_string: vec![KerberosString(Ia5String::new(&self.client_name).unwrap())],
                }),
                realm: KerberosString(Ia5String::new("EXAMPLE.COM").unwrap()),
                sname: Some(PrincipalName {
                    name_type: 2,
                    name_string: vec![
                        KerberosString(Ia5String::new(&self.service_name).unwrap()),
                        KerberosString(Ia5String::new("EXAMPLE.COM").unwrap()),
                    ],
                }),
                from: self.from.map(|t| {
                    KerberosTime::from_system_time(t)
                        .expect("Failed to build KerberosTime from SystemTime")
                }),
                till: KerberosTime::from_system_time(self.until)
                    .expect("Failed to build KerberosTime from SystemTime"),
                rtime: self.renew.map(|t| {
                    KerberosTime::from_system_time(t)
                        .expect("Failed to build KerberosTime from SystemTime")
                }),
                nonce: self.nonce,
                etype: vec![
                    EncryptionType::AES256_CTS_HMAC_SHA1_96 as i32,
                    // MIT KRB5 claims to support these values, but if they are provided then MIT
                    // KDC's will ignore them.
                    // EncryptionType::AES128_CTS_HMAC_SHA256_128 as i32,
                    // EncryptionType::AES256_CTS_HMAC_SHA384_192 as i32,
                ],
                addresses: None,
                enc_authorization_data: None,
                additional_tickets: None,
            },
        }
    }
}

impl TryFrom<KdcRep> for KerberosAsRep {
    type Error = KrbError;

    fn try_from(rep: KdcRep) -> Result<Self, Self::Error> {
        // assert the pvno and msg_type
        if rep.pvno != 5 {
            todo!();
        }

        let msg_type = KrbMessageType::try_from(rep.msg_type).map_err(|_| {
            KrbError::InvalidEnumValue(
                std::any::type_name::<KrbMessageType>().to_string(),
                rep.msg_type as i32,
            )
        })?;

        match msg_type {
            KrbMessageType::KrbAsRep => {
                let enc_part = EncryptedData::try_from(rep.enc_part)?;
                trace!(?enc_part);

                let client_realm: String = rep.crealm.into();
                let client_name: String = rep.cname.into();

                Ok(KerberosAsRep {
                    client_realm,
                    client_name,
                    enc_part,
                })
            }
            _ => Err(KrbError::InvalidMessageType(
                rep.msg_type as i32,
                KrbMessageType::KrbAsRep as i32,
            )),
        }
    }
}

impl TryFrom<KdcRep> for KerberosTgsRep {
    type Error = KrbError;

    fn try_from(rep: KdcRep) -> Result<Self, Self::Error> {
        // assert the pvno and msg_type
        if rep.pvno != 5 {
            todo!();
        }

        let msg_type = KrbMessageType::try_from(rep.msg_type).map_err(|_| {
            KrbError::InvalidEnumValue(
                std::any::type_name::<KrbMessageType>().to_string(),
                rep.msg_type as i32,
            )
        })?;

        match msg_type {
            KrbMessageType::KrbTgsRep => Ok(KerberosTgsRep {}),
            _ => Err(KrbError::InvalidMessageType(
                rep.msg_type as i32,
                KrbMessageType::KrbTgsRep as i32,
            )),
        }
    }
}

impl TryFrom<crate::asn1::krb_error::KrbError> for KerberosErrRep {
    type Error = KrbError;

    fn try_from(rep: crate::asn1::krb_error::KrbError) -> Result<Self, Self::Error> {
        // assert the pvno and msg_type
        if rep.pvno != 5 {
            todo!();
        }

        let msg_type = KrbMessageType::try_from(rep.msg_type).map_err(|_| {
            KrbError::InvalidEnumValue(
                std::any::type_name::<KrbMessageType>().to_string(),
                rep.msg_type as i32,
            )
        })?;

        match msg_type {
            KrbMessageType::KrbError => {
                let error_code = KrbErrorCode::try_from(rep.error_code).map_err(|_| {
                    KrbError::InvalidEnumValue(
                        std::any::type_name::<KrbErrorCode>().to_string(),
                        rep.error_code,
                    )
                })?;

                let rep = match error_code {
                    KrbErrorCode::KdcErrPreauthRequired => {
                        let edata = rep.error_data.ok_or(KrbError::MissingPaData)?;

                        let pavec: Vec<PaData> = MethodData::from_der(edata.as_bytes())
                            .map_err(|_| KrbError::DerDecodePaData)?;

                        let pa_rep = KerberosPaRep::try_from(pavec)?;
                        KerberosErrRep::Pa(pa_rep)
                    }
                    err_code => KerberosErrRep::Err(err_code),
                };

                Ok(rep)
            }
            _ => Err(KrbError::InvalidMessageType(
                rep.msg_type as i32,
                KrbMessageType::KrbError as i32,
            )),
        }
    }
}

impl TryFrom<Vec<PaData>> for KerberosPaRep {
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

        Ok(KerberosPaRep {
            pa_fx_fast,
            pa_fx_cookie,
            enc_timestamp,
            etype_info2,
        })
    }
}

impl EncryptedData {
    pub fn derive_key(
        &self,
        passphrase: &[u8],
        realm: &[u8],
        cname: &[u8],
    ) -> Result<BaseKey, KrbError> {
        match self {
            EncryptedData::Aes256CtsHmacSha196 { .. } => {
                // todo! there is some way to get a number of rounds here
                // but I can't obviously see it?
                let iter_count = None;
                derive_key_aes256_cts_hmac_sha1_96(passphrase, realm, cname, iter_count)
                    .map(|k| BaseKey::Aes256 { k })
            }
        }
    }

    pub fn decrypt_data(&self, base_key: &BaseKey, key_usage: i32) -> Result<Vec<u8>, KrbError> {
        match (self, base_key) {
            (EncryptedData::Aes256CtsHmacSha196 { kvno: _, data }, BaseKey::Aes256 { k }) => {
                decrypt_aes256_cts_hmac_sha1_96(&k, &data, key_usage)
            }
        }
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
