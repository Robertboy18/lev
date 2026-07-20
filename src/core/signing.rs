//! Ed25519 keys and detached signatures for lev manifests.
//!
//! There is no trust-on-first-use: callers provide the public key. Records use
//! a small versioned text format:
//!
//! ```text
//! LEV-ED25519-PRIVATE-KEY-V1:<base64 seed>
//! LEV-ED25519-PUBLIC-KEY-V1:<base64 public key>
//! LEV-ED25519-SIGNATURE-V1:<base64 signature>
//! ```
//!
//! Private records contain the 32-byte RFC 8032 seed. On Unix they must have
//! mode `0600`; public keys have no special permission requirement.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::core::atomic_file::rename_replace;

const PRIVATE_PREFIX: &str = "LEV-ED25519-PRIVATE-KEY-V1:";
const PUBLIC_PREFIX: &str = "LEV-ED25519-PUBLIC-KEY-V1:";
const SIGNATURE_PREFIX: &str = "LEV-ED25519-SIGNATURE-V1:";
const MAX_KEY_FILE_BYTES: u64 = 4 * 1024;

/// Signing authority loaded from a private key file.
pub struct ManifestSigner(SigningKey);

/// Explicit trust anchor loaded from a public key file.
pub struct ManifestVerifier(VerifyingKey);

/// Public details returned after generating a new key pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedKeyPair {
    /// Stable SHA-256 fingerprint of the 32-byte public key.
    pub fingerprint: String,
}

impl ManifestSigner {
    /// Load and validate a versioned Ed25519 private-key file.
    pub fn load(path: &Path) -> Result<Self> {
        validate_private_permissions(path)?;
        let bytes = read_small_file(path)?;
        let seed = decode_record::<32>(&bytes, PRIVATE_PREFIX, "private key")?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// Sign the exact bytes that will be published as a remote manifest.
    pub fn sign(&self, message: &[u8]) -> String {
        let signature: Signature = self.0.sign(message);
        format!(
            "{SIGNATURE_PREFIX}{}\n",
            STANDARD_NO_PAD.encode(signature.to_bytes())
        )
    }

    /// Fingerprint of the public half, useful for logs and audit output.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.0.verifying_key())
    }
}

impl ManifestVerifier {
    /// Load and validate a versioned Ed25519 public-key file.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = read_small_file(path)?;
        let encoded = decode_record::<32>(&bytes, PUBLIC_PREFIX, "public key")?;
        let key = VerifyingKey::from_bytes(&encoded).context("invalid Ed25519 public key")?;
        Ok(Self(key))
    }

    /// Verify a detached, versioned signature over `message`.
    pub fn verify(&self, message: &[u8], signature_record: &[u8]) -> Result<()> {
        let encoded =
            decode_record::<64>(signature_record, SIGNATURE_PREFIX, "manifest signature")?;
        let signature = Signature::from_bytes(&encoded);
        self.0
            .verify_strict(message, &signature)
            .context("remote-cache manifest signature is invalid")
    }

    /// Stable SHA-256 fingerprint of this trust anchor.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.0)
    }
}

/// Generate a signing key and its corresponding public trust anchor.
///
/// Both files are staged before either destination is replaced. Existing
/// destinations are refused unless `force` is true, which prevents an
/// accidental key rotation from making older cache snapshots unverifiable.
pub fn generate_key_pair(
    private_path: &Path,
    public_path: &Path,
    force: bool,
) -> Result<GeneratedKeyPair> {
    if private_path == public_path {
        bail!("private and public key paths must be different");
    }
    if !force {
        for path in [private_path, public_path] {
            if path.exists() {
                bail!(
                    "{} already exists; pass --force to replace the key pair",
                    path.display()
                );
            }
        }
    }

    ensure_parent(private_path)?;
    ensure_parent(public_path)?;
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();
    let private_record = format!(
        "{PRIVATE_PREFIX}{}\n",
        STANDARD_NO_PAD.encode(signing_key.to_bytes())
    );
    let public_record = format!(
        "{PUBLIC_PREFIX}{}\n",
        STANDARD_NO_PAD.encode(verifying_key.to_bytes())
    );

    let private_temporary = stage_file(private_path, private_record.as_bytes(), true)?;
    let public_temporary = match stage_file(public_path, public_record.as_bytes(), false) {
        Ok(path) => path,
        Err(error) => {
            let _ = fs::remove_file(&private_temporary);
            return Err(error);
        }
    };

    // Publish the shareable key first. If the process is interrupted between
    // renames, no private key is left without its matching public key.
    replace_staged(&public_temporary, public_path, force)?;
    if let Err(error) = replace_staged(&private_temporary, private_path, force) {
        let _ = fs::remove_file(&private_temporary);
        return Err(error);
    }
    Ok(GeneratedKeyPair {
        fingerprint: fingerprint(&verifying_key),
    })
}

fn decode_record<const N: usize>(bytes: &[u8], prefix: &str, label: &str) -> Result<[u8; N]> {
    let text = std::str::from_utf8(bytes)
        .with_context(|| format!("{label} file is not UTF-8"))?
        .trim();
    let payload = text
        .strip_prefix(prefix)
        .with_context(|| format!("unsupported or malformed {label} format"))?;
    if payload.is_empty() || payload.chars().any(char::is_whitespace) {
        bail!("malformed {label} record");
    }
    let decoded = STANDARD_NO_PAD
        .decode(payload)
        .with_context(|| format!("{label} contains invalid base64"))?;
    decoded.try_into().map_err(|value: Vec<u8>| {
        anyhow::anyhow!("{label} has {} decoded bytes, expected {N}", value.len())
    })
}

fn read_small_file(path: &Path) -> Result<Vec<u8>> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if metadata.len() > MAX_KEY_FILE_BYTES {
        bail!("{} is too large to be a lev key file", path.display());
    }
    fs::read(path).with_context(|| format!("failed to read {}", path.display()))
}

fn ensure_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))
}

fn stage_file(destination: &Path, contents: &[u8], private: bool) -> Result<PathBuf> {
    for _ in 0..32 {
        let temporary = temporary_path(destination);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_private_mode(&mut options, private);
        match options.open(&temporary) {
            Ok(mut file) => {
                file.write_all(contents)
                    .with_context(|| format!("failed to write {}", temporary.display()))?;
                file.sync_all()
                    .with_context(|| format!("failed to sync {}", temporary.display()))?;
                return Ok(temporary);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", temporary.display()));
            }
        }
    }
    bail!(
        "failed to allocate a temporary key file beside {}",
        destination.display()
    )
}

fn temporary_path(destination: &Path) -> PathBuf {
    let mut random = [0_u8; 8];
    OsRng.fill_bytes(&mut random);
    let suffix = crate::cache::lowercase_hex(&random);
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    destination.with_file_name(format!(".{name}.lev-tmp-{suffix}"))
}

fn replace_staged(staged: &Path, destination: &Path, force: bool) -> Result<()> {
    if !force && destination.exists() {
        bail!("{} appeared while generating keys", destination.display());
    }
    rename_replace(staged, destination)
        .with_context(|| format!("failed to publish {}", destination.display()))
}

#[cfg(unix)]
fn set_private_mode(options: &mut OpenOptions, private: bool) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(if private { 0o600 } else { 0o644 });
}

#[cfg(not(unix))]
fn set_private_mode(_options: &mut OpenOptions, _private: bool) {}

#[cfg(unix)]
fn validate_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata =
        fs::metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.mode() & 0o077 != 0 {
        bail!(
            "{} is readable or writable by group/other users; run `chmod 600 {}`",
            path.display(),
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn fingerprint(key: &VerifyingKey) -> String {
    format!("{:x}", Sha256::digest(key.to_bytes()))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;

    use tempfile::tempdir;

    use super::{ManifestSigner, ManifestVerifier, generate_key_pair};

    #[test]
    fn generated_pair_signs_and_detects_tampering() {
        let temp = tempdir().unwrap();
        let private = temp.path().join("signing.key");
        let public = temp.path().join("signing.pub");
        let generated = generate_key_pair(&private, &public, false).unwrap();
        let signer = ManifestSigner::load(&private).unwrap();
        let verifier = ManifestVerifier::load(&public).unwrap();
        assert_eq!(generated.fingerprint, signer.fingerprint());
        assert_eq!(generated.fingerprint, verifier.fingerprint());

        let signature = signer.sign(b"manifest");
        verifier.verify(b"manifest", signature.as_bytes()).unwrap();
        let error = verifier
            .verify(b"changed", signature.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(error.contains("signature is invalid"), "{error}");
    }

    #[test]
    fn refuses_accidental_overwrite_and_wrong_public_key() {
        let temp = tempdir().unwrap();
        let private = temp.path().join("signing.key");
        let public = temp.path().join("signing.pub");
        generate_key_pair(&private, &public, false).unwrap();
        let error = generate_key_pair(&private, &public, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists"), "{error}");

        let other_private = temp.path().join("other.key");
        let other_public = temp.path().join("other.pub");
        generate_key_pair(&other_private, &other_public, false).unwrap();
        let signature = ManifestSigner::load(&private).unwrap().sign(b"manifest");
        let error = ManifestVerifier::load(&other_public)
            .unwrap()
            .verify(b"manifest", signature.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(error.contains("signature is invalid"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_permissive_private_key_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().unwrap();
        let private = temp.path().join("signing.key");
        let public = temp.path().join("signing.pub");
        generate_key_pair(&private, &public, false).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o644)).unwrap();
        let error = ManifestSigner::load(&private).err().unwrap().to_string();
        assert!(error.contains("chmod 600"), "{error}");
    }
}
