use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};

use crate::config::Config;

#[derive(Debug, Clone)]
pub struct AuthorityKeyPair {
    pub public: [u8; 32],
    pub secret: [u8; 32],
    pub cert_validity: Duration,
}

impl AuthorityKeyPair {
    pub fn load(config: &Config) -> anyhow::Result<Self> {
        let public_path = config
            .sv2_authority_public_key_path
            .as_deref()
            .ok_or_else(|| anyhow!("sv2.authority_public_key_path is required"))?;
        let secret_path = config
            .sv2_authority_secret_key_path
            .as_deref()
            .ok_or_else(|| anyhow!("sv2.authority_secret_key_path is required"))?;

        Ok(Self {
            public: read_public_hex_key(public_path).context("load sv2 authority public key")?,
            secret: read_secret_hex_key(secret_path).context("load sv2 authority secret key")?,
            cert_validity: Duration::from_secs(config.sv2_cert_validity_secs),
        })
    }

    pub fn responder(&self) -> anyhow::Result<Box<noise_sv2::Responder>> {
        noise_sv2::Responder::from_authority_kp(&self.public, &self.secret, self.cert_validity)
            .map_err(|err| anyhow!("build sv2 noise responder: {err:?}"))
    }
}

fn read_public_hex_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    read_hex_key(path)
}

fn read_secret_hex_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    #[cfg(unix)]
    reject_insecure_unix_permissions(path)?;

    read_hex_key(path)
}

fn read_hex_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let normalized = raw
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches("0x")
        .to_string();
    let bytes = hex::decode(&normalized).with_context(|| format!("decode {}", path.display()))?;
    if bytes.len() != 32 {
        bail!("{} must contain exactly 32 bytes of hex", path.display());
    }
    Ok(bytes.try_into().expect("validated key length"))
}

#[cfg(unix)]
fn reject_insecure_unix_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} permissions must not be readable or writable by group/other; use chmod 600",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{read_public_hex_key, read_secret_hex_key};

    #[test]
    fn rejects_non_32_byte_key() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "stratumbee-sv2-short-key-{}.hex",
            std::process::id()
        ));
        std::fs::write(&path, "abcd").expect("write test key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("tighten test key permissions");
        }

        let err = read_public_hex_key(&path).expect_err("short key should fail");
        assert!(format!("{err:#}").contains("exactly 32 bytes"));

        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn allows_public_key_with_world_readable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "stratumbee-sv2-public-key-{}.hex",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .expect("write public key");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("set public key permissions");

        read_public_hex_key(&path).expect("public key should allow 0644");

        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_secret_key_with_world_readable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "stratumbee-sv2-secret-key-{}.hex",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .expect("write secret key");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("set secret key permissions");

        let err = read_secret_hex_key(&path).expect_err("secret key should reject 0644");
        assert!(format!("{err:#}").contains("chmod 600"));

        let _ = std::fs::remove_file(path);
    }
}
