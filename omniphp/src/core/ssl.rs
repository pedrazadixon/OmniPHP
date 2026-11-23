//! Local HTTPS: one certificate authority, trusted once, signs a certificate
//! per site. Adding a site therefore never needs a new trust prompt.
//!
//! The authority's private key sits on disk next to the app, so the authority
//! is *name-constrained*: it is only valid for development suffixes such as
//! `.test`. Even if the key leaked, it could not be used to impersonate real
//! websites to this PC.

use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, SystemTime};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, NameConstraints,
};
use sha1::{Digest, Sha1};
use time::{Duration, OffsetDateTime};

/// Top-level names the authority may sign for.
pub const ALLOWED_SUFFIXES: [&str; 4] = ["test", "localhost", "local", "internal"];

const CA_NAME: &str = "OmniPHP Local Development CA";
const LEAF_VALID_DAYS: i64 = 800;
/// Re-issue a site certificate well before it expires.
const LEAF_RENEW_AFTER_DAYS: u64 = 700;

pub struct Authority {
    issuer: Issuer<'static, KeyPair>,
    thumbprint: String,
}

/// Certificate and key files for one site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteCertificate {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl Authority {
    /// Loads the authority from `dir`, creating it on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let cert_file = dir.join("ca.crt");
        let key_file = dir.join("ca.key");
        if !authority_exists(dir) {
            std::fs::create_dir_all(dir)?;
            let (cert_pem, key_pem) = generate_authority()?;
            std::fs::write(&key_file, key_pem)
                .with_context(|| format!("cannot write {}", key_file.display()))?;
            std::fs::write(&cert_file, cert_pem)
                .with_context(|| format!("cannot write {}", cert_file.display()))?;
        }
        let cert_pem = std::fs::read_to_string(&cert_file)?;
        let key = KeyPair::from_pem(&std::fs::read_to_string(&key_file)?)
            .context("the certificate authority key is unreadable")?;
        Ok(Self {
            issuer: Issuer::from_ca_cert_pem(&cert_pem, key)
                .context("the certificate authority certificate is unreadable")?,
            thumbprint: thumbprint_of_pem(&cert_pem)?,
        })
    }

    /// SHA-1 thumbprint, the way Windows identifies a certificate.
    pub fn thumbprint(&self) -> &str {
        &self.thumbprint
    }

    /// Makes sure `dir` holds a current certificate for `hostname` (plus
    /// `*.hostname` and any aliases). Returns `None` when the hostname is
    /// outside the suffixes this authority may sign, in which case the site is
    /// served over plain HTTP.
    pub fn ensure_site(&self, hostname: &str, aliases: &[String], dir: &Path) -> Result<Option<SiteCertificate>> {
        if !covers(hostname) {
            return Ok(None);
        }
        let mut names = vec![hostname.to_string()];
        if hostname.parse::<std::net::IpAddr>().is_err() && hostname != "localhost" {
            names.push(format!("*.{hostname}"));
        }
        for alias in aliases {
            // One name outside the constraints would invalidate the whole certificate.
            if covers(alias) && !names.contains(alias) {
                names.push(alias.clone());
            }
        }

        std::fs::create_dir_all(dir)?;
        let files = SiteCertificate {
            cert: dir.join(format!("{hostname}.crt")),
            key: dir.join(format!("{hostname}.key")),
        };
        // Records what the certificate was issued for, so a changed alias list
        // or a new authority triggers a re-issue.
        let stamp_file = dir.join(format!("{hostname}.issued"));
        let stamp = format!("{}\n{}\n", self.thumbprint, names.join(" "));
        let fresh = files.cert.is_file()
            && files.key.is_file()
            && std::fs::read_to_string(&stamp_file).is_ok_and(|saved| saved == stamp)
            && age_of(&files.cert).is_some_and(|age| age < StdDuration::from_secs(LEAF_RENEW_AFTER_DAYS * 86_400));
        if fresh {
            return Ok(Some(files));
        }

        let mut params = CertificateParams::new(names).context("invalid hostname for a certificate")?;
        params.distinguished_name.push(DnType::CommonName, hostname);
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::days(1);
        params.not_after = now + Duration::days(LEAF_VALID_DAYS);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.issuer)?;

        std::fs::write(&files.key, key.serialize_pem())?;
        std::fs::write(&files.cert, cert.pem())?;
        std::fs::write(&stamp_file, stamp)?;
        Ok(Some(files))
    }
}

/// Whether an authority has already been created in `dir`.
pub fn authority_exists(dir: &Path) -> bool {
    dir.join("ca.crt").is_file() && dir.join("ca.key").is_file()
}

/// Whether the authority is allowed to sign for `name`.
pub fn covers(name: &str) -> bool {
    let name = name.strip_prefix("*.").unwrap_or(name).to_ascii_lowercase();
    if name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
        return true;
    }
    name.rsplit('.').next().is_some_and(|suffix| ALLOWED_SUFFIXES.contains(&suffix))
}

/// Upper-case hex SHA-1 of the certificate's DER bytes.
pub fn thumbprint_of_pem(cert_pem: &str) -> Result<String> {
    let der = pem::parse(cert_pem).context("not a PEM certificate")?;
    Ok(Sha1::digest(der.contents()).iter().map(|byte| format!("{byte:02X}")).collect())
}

fn generate_authority() -> Result<(String, String)> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.distinguished_name.push(DnType::CommonName, CA_NAME);
    params.distinguished_name.push(DnType::OrganizationName, "OmniPHP");
    // May sign site certificates only, never further authorities.
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: ALLOWED_SUFFIXES
            .into_iter()
            .map(|suffix| GeneralSubtree::DnsName(suffix.to_string()))
            .collect(),
        excluded_subtrees: Vec::new(),
    });
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(3650);
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

fn age_of(file: &Path) -> Option<StdDuration> {
    SystemTime::now().duration_since(file.metadata().ok()?.modified().ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_only_development_names() {
        assert!(covers("shop.test"));
        assert!(covers("*.shop.test"));
        assert!(covers("localhost"));
        assert!(covers("api.shop.LOCALHOST"));
        assert!(covers("127.0.0.1"));
        assert!(!covers("example.com"));
        assert!(!covers("192.168.1.10"));
        assert!(!covers("test.com"));
    }

    #[test]
    fn authority_is_created_once_and_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let first = Authority::load_or_create(tmp.path()).unwrap();
        let second = Authority::load_or_create(tmp.path()).unwrap();
        assert_eq!(first.thumbprint(), second.thumbprint());
        assert_eq!(first.thumbprint().len(), 40);
    }

    #[test]
    fn site_certificate_is_reused_until_its_names_change() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = Authority::load_or_create(tmp.path()).unwrap();
        let sites = tmp.path().join("sites");

        let files = authority.ensure_site("shop.test", &[], &sites).unwrap().unwrap();
        let original = std::fs::read(&files.cert).unwrap();
        assert!(String::from_utf8_lossy(&original).contains("BEGIN CERTIFICATE"));

        authority.ensure_site("shop.test", &[], &sites).unwrap();
        assert_eq!(std::fs::read(&files.cert).unwrap(), original, "unchanged site keeps its certificate");

        authority.ensure_site("shop.test", &["store.test".into()], &sites).unwrap();
        assert_ne!(std::fs::read(&files.cert).unwrap(), original, "a new alias re-issues");
    }

    #[test]
    fn names_outside_the_constraints_get_no_certificate() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = Authority::load_or_create(tmp.path()).unwrap();
        assert_eq!(authority.ensure_site("shop.example.com", &[], tmp.path()).unwrap(), None);
    }
}
