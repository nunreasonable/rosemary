//! CMS integrity and code-signing certificate validation are separate operations.
use super::code_directory::{INVALID, MODIFIED, UNSUPPORTED};
use cms::{
    cert::CertificateChoices,
    content_info::ContentInfo,
    signed_data::{SignedData, SignerIdentifier},
};
use der::{
    Decode, Encode,
    asn1::{ObjectIdentifier, OctetString},
};
use ring::{digest, signature};
use rustls_pki_types::UnixTime;

pub(super) struct Signer {
    pub certificates: Vec<Vec<u8>>,
    pub directory_hashes: Vec<(String, Vec<u8>)>,
}

const ROOTS: &[&[u8]] = &[
    include_bytes!("apple_roots/apple-inc.der"),
    include_bytes!("apple_roots/apple-g2.der"),
    include_bytes!("apple_roots/apple-g3.der"),
];

/// Authenticate detached CodeDirectory content, including signed attributes.
pub(super) fn verify(cms: &[u8], content: &[u8]) -> Result<Signer, i32> {
    let normalized = super::code_ber::normalize(cms)?;
    let info = ContentInfo::from_der(&normalized).map_err(|_| INVALID)?;
    if info.content_type.to_string() != "1.2.840.113549.1.7.2" {
        return Err(INVALID);
    }
    let signed: SignedData = info.content.decode_as().map_err(|_| INVALID)?;
    if signed.encap_content_info.econtent_type.to_string() != "1.2.840.113549.1.7.1" {
        return Err(UNSUPPORTED);
    }
    if let Some(embedded) = &signed.encap_content_info.econtent {
        let embedded: OctetString = embedded.decode_as().map_err(|_| INVALID)?;
        if embedded.as_bytes() != content {
            return Err(MODIFIED);
        }
    }
    if signed.signer_infos.0.len() != 1 {
        return Err(UNSUPPORTED);
    }
    let signer = signed.signer_infos.0.get(0).ok_or(INVALID)?;
    let certificates = signed.certificates.as_ref().ok_or(INVALID)?;
    let mut chain = Vec::new();
    let mut leaf = None;
    for cert in certificates.0.iter() {
        let CertificateChoices::Certificate(cert) = cert else {
            return Err(UNSUPPORTED);
        };
        let matches = match &signer.sid {
            SignerIdentifier::IssuerAndSerialNumber(id) => {
                id.issuer == cert.tbs_certificate.issuer
                    && id.serial_number == cert.tbs_certificate.serial_number
            }
            _ => return Err(UNSUPPORTED),
        };
        if matches {
            if leaf.replace(cert).is_some() {
                return Err(INVALID);
            }
        } else {
            chain.push(cert.to_der().map_err(|_| INVALID)?);
        }
    }
    let leaf = leaf.ok_or(INVALID)?;
    let hash = match signer.digest_alg.oid.to_string().as_str() {
        "2.16.840.1.101.3.4.2.1" => &digest::SHA256,
        "2.16.840.1.101.3.4.2.2" => &digest::SHA384,
        "2.16.840.1.101.3.4.2.3" => &digest::SHA512,
        _ => return Err(UNSUPPORTED),
    };
    if !signed
        .digest_algorithms
        .iter()
        .any(|a| a == &signer.digest_alg)
    {
        return Err(INVALID);
    }
    let mut directory_hashes = Vec::new();
    let data = if let Some(attrs) = &signer.signed_attrs {
        let mut found_digest = false;
        let mut found_type = false;
        for attr in attrs.iter() {
            match attr.oid.to_string().as_str() {
                "1.2.840.113549.1.9.4" => {
                    if found_digest || attr.values.len() != 1 {
                        return Err(INVALID);
                    }
                    found_digest = true;
                    let value: OctetString = attr
                        .values
                        .get(0)
                        .ok_or(INVALID)?
                        .decode_as()
                        .map_err(|_| INVALID)?;
                    if value.as_bytes() != digest::digest(hash, content).as_ref() {
                        return Err(MODIFIED);
                    }
                }
                "1.2.840.113549.1.9.3" => {
                    if found_type || attr.values.len() != 1 {
                        return Err(INVALID);
                    }
                    found_type = true;
                    let value: ObjectIdentifier = attr
                        .values
                        .get(0)
                        .ok_or(INVALID)?
                        .decode_as()
                        .map_err(|_| INVALID)?;
                    if value != signed.encap_content_info.econtent_type {
                        return Err(INVALID);
                    }
                }
                "1.2.840.113635.100.9.2" => {
                    #[derive(der::Sequence)]
                    struct Agility {
                        algorithm: ObjectIdentifier,
                        digest: OctetString,
                    }
                    if !directory_hashes.is_empty() {
                        return Err(INVALID);
                    }
                    for value in attr.values.iter() {
                        let value: Agility = value.decode_as().map_err(|_| INVALID)?;
                        directory_hashes.push((
                            value.algorithm.to_string(),
                            value.digest.as_bytes().to_vec(),
                        ));
                    }
                }
                _ => (),
            }
        }
        if !found_digest || !found_type {
            return Err(INVALID);
        }
        attrs.to_der().map_err(|_| INVALID)?
    } else {
        content.to_vec()
    };
    let key = &leaf.tbs_certificate.subject_public_key_info;
    let signature_algorithm: &dyn signature::VerificationAlgorithm = match (
        key.algorithm.oid.to_string().as_str(),
        signer.signature_algorithm.oid.to_string().as_str(),
        hash,
    ) {
        ("1.2.840.113549.1.1.1", "1.2.840.113549.1.1.1" | "1.2.840.113549.1.1.11", h)
            if h == &digest::SHA256 =>
        {
            &signature::RSA_PKCS1_2048_8192_SHA256
        }
        ("1.2.840.113549.1.1.1", "1.2.840.113549.1.1.1" | "1.2.840.113549.1.1.12", h)
            if h == &digest::SHA384 =>
        {
            &signature::RSA_PKCS1_2048_8192_SHA384
        }
        ("1.2.840.113549.1.1.1", "1.2.840.113549.1.1.1" | "1.2.840.113549.1.1.13", h)
            if h == &digest::SHA512 =>
        {
            &signature::RSA_PKCS1_2048_8192_SHA512
        }
        _ => return Err(UNSUPPORTED),
    };
    signature::UnparsedPublicKey::new(
        signature_algorithm,
        key.subject_public_key.as_bytes().ok_or(INVALID)?,
    )
    .verify(&data, signer.signature.as_bytes())
    .map_err(|_| MODIFIED)?;
    let mut ordered = vec![leaf.to_der().map_err(|_| INVALID)?];
    let mut current = leaf.clone();
    while current.tbs_certificate.issuer != current.tbs_certificate.subject {
        let mut parent = None;
        for (index, bytes) in chain.iter().enumerate() {
            let cert = x509_cert::Certificate::from_der(bytes).map_err(|_| INVALID)?;
            if cert.tbs_certificate.subject == current.tbs_certificate.issuer {
                if parent.is_some() {
                    return Err(UNSUPPORTED);
                }
                parent = Some((index, cert));
            }
        }
        let Some((index, cert)) = parent else {
            break;
        };
        let bytes = chain.remove(index);
        if ordered.contains(&bytes) {
            return Err(INVALID);
        }
        ordered.push(bytes);
        current = cert;
    }
    Ok(Signer {
        certificates: ordered,
        directory_hashes,
    })
}

impl Signer {
    pub fn verify_apple_chain(&self) -> Result<(), i32> {
        self.verify_chain(ROOTS, UnixTime::now())
    }

    fn verify_chain(&self, roots: &[&[u8]], time: UnixTime) -> Result<(), i32> {
        use openssl::{
            stack::Stack,
            x509::{
                X509, X509StoreContext,
                store::X509StoreBuilder,
                verify::{X509VerifyFlags, X509VerifyParam},
            },
        };
        // Only these Apple markers have a fully understood critical payload.
        for (index, bytes) in self.certificates.iter().enumerate() {
            let cert = x509_cert::Certificate::from_der(bytes).map_err(|_| INVALID)?;
            let mut usage = false;
            for ext in cert.tbs_certificate.extensions.iter().flatten() {
                let oid = ext.extn_id.to_string();
                if oid == "2.5.29.37" {
                    let usages =
                        x509_cert::ext::pkix::ExtendedKeyUsage::from_der(ext.extn_value.as_bytes())
                            .map_err(|_| INVALID)?;
                    usage = usages
                        .0
                        .iter()
                        .any(|oid| oid.to_string() == "1.3.6.1.5.5.7.3.3");
                    if !usage {
                        return Err(-66996);
                    }
                }
                if ext.critical {
                    match oid.as_str() {
                        "2.5.29.19" | "2.5.29.15" | "2.5.29.37" => (),
                        "1.2.840.113635.100.6.1.13" | "1.2.840.113635.100.6.2.6" => {
                            if ext.extn_value.as_bytes() != [5, 0] {
                                return Err(INVALID);
                            }
                        }
                        _ => return Err(UNSUPPORTED),
                    }
                }
            }
            if index == 0 && !usage {
                return Err(-66996);
            }
        }
        let mut store = X509StoreBuilder::new().map_err(|_| INVALID)?;
        for root in roots {
            store
                .add_cert(X509::from_der(root).map_err(|_| INVALID)?)
                .map_err(|_| INVALID)?;
        }
        let mut params = X509VerifyParam::new().map_err(|_| INVALID)?;
        params.set_time(time.as_secs().try_into().map_err(|_| INVALID)?);
        // Critical extensions were checked above; OpenSSL still enforces KU and CA constraints.
        params
            .set_flags(X509VerifyFlags::IGNORE_CRITICAL | X509VerifyFlags::X509_STRICT)
            .map_err(|_| INVALID)?;
        store.set_param(&params).map_err(|_| INVALID)?;
        let leaf =
            X509::from_der(self.certificates.first().ok_or(INVALID)?).map_err(|_| INVALID)?;
        let mut intermediates = Stack::new().map_err(|_| INVALID)?;
        for bytes in &self.certificates[1..] {
            intermediates
                .push(X509::from_der(bytes).map_err(|_| INVALID)?)
                .map_err(|_| INVALID)?;
        }
        let mut context = X509StoreContext::new().map_err(|_| INVALID)?;
        let valid = context
            .init(&store.build(), &leaf, &intermediates, |context| {
                context.verify_cert()
            })
            .map_err(|_| -66996)?;
        if !valid {
            return Err(-66996);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires ROSEMARY_SIGNING_TEST_IMAGE naming an external signed Mach-O"]
    fn verifies_external_macho_signature() {
        let path = std::env::var_os("ROSEMARY_SIGNING_TEST_IMAGE").expect("fixture path");
        let data = std::fs::read(path).unwrap();
        let image = crate::code_directory::architecture(&data, 0x1000007).expect("x86_64 slice");
        let (offset, blob) = crate::code_directory::signature(image).expect("signature");
        let parts = crate::code_directory::components(blob).unwrap();
        let cd = crate::code_directory::primary(blob).unwrap();
        cd.verify_pages(image, offset).expect("page hashes");
        eprintln!("page hashes verified; {} signature components", parts.len());
        let cms = parts.get(&0x10000).expect("CMS");
        let signer = verify(&cms[8..], cd.bytes).expect("CMS integrity");
        eprintln!("CMS verified; {} certificates", signer.certificates.len());
        signer
            .verify_apple_chain()
            .expect("Apple code signing chain");
    }

    #[test]
    fn verifies_cms_content_and_rejects_tampering() {
        let cms = include_bytes!("testdata/codesign.cms");
        let signer = verify(cms, b"code directory fixture").unwrap();
        assert_eq!(verify(cms, b"Code directory fixture").err(), Some(MODIFIED));
        assert_eq!(signer.verify_apple_chain(), Err(-66996));
        let leaf = x509_cert::Certificate::from_der(&signer.certificates[0]).unwrap();
        let time = leaf.tbs_certificate.validity.not_before.to_unix_duration()
            + std::time::Duration::from_secs(60);
        assert_eq!(
            signer.verify_chain(
                &[include_bytes!("testdata/codesign-root.der")],
                UnixTime::since_unix_epoch(time)
            ),
            Ok(())
        );
        assert!(
            signer
                .verify_chain(
                    &[include_bytes!("testdata/codesign-root.der")],
                    UnixTime::since_unix_epoch(std::time::Duration::ZERO)
                )
                .is_err()
        );
        for end in [0, 1, 20, cms.len() - 1] {
            assert!(verify(&cms[..end], b"code directory fixture").is_err());
        }
        let mut damaged = cms.to_vec();
        let pos = damaged.len() - 20;
        damaged[pos] ^= 1;
        assert!(verify(&damaged, b"code directory fixture").is_err());
    }
}
