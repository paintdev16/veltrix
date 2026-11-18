//! Offline signing utility. The input manifest must already describe the release.
use ed25519_dalek::{Signer, SigningKey};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
};
use zeroize::Zeroizing;

fn sign(
    key_path: &Path,
    manifest_path: &Path,
    package_path: &Path,
    signature_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut key = Zeroizing::new([0u8; 32]);
    let mut key_file = fs::File::open(key_path)?;
    key_file.read_exact(&mut *key)?;
    let mut extra = [0];
    if key_file.read(&mut extra)? != 0 {
        return Err("signing key must be exactly 32 raw bytes".into());
    }
    let mut manifest = Vec::new();
    fs::File::open(manifest_path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut manifest)?;
    if manifest.len() > 1024 * 1024 {
        return Err("manifest too large".into());
    }
    let description: veltrix_core::Manifest = serde_json::from_slice(&manifest)?;
    if description.app_id.is_empty()
        || description
            .installed_sha256
            .as_deref()
            .is_none_or(|hash| hex::decode(hash).map_or(true, |bytes| bytes.len() != 32))
    {
        return Err(
            "manifest must contain an app identity and installed executable SHA-256".into(),
        );
    }
    let signer = SigningKey::from_bytes(&key);
    let signature = signer.sign(&manifest);
    let policy = veltrix_core::Policy {
        app_id: description.app_id.clone(),
        channel: description.channel,
        current_version: "0.0.0-0".parse()?,
        platform: description.platform.clone(),
        architecture: description.architecture.clone(),
        public_key: signer.verifying_key().to_bytes(),
        max_package_size: description.size,
    };
    let verified = policy.verify_manifest(&manifest, &signature.to_bytes())?;
    veltrix_core::verify_package(package_path, &verified)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(signature_path)?;
    output.write_all(&signature.to_bytes())?;
    output.sync_all()?;
    eprintln!(
        "Public key: {}",
        hex::encode(signer.verifying_key().to_bytes())
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 4 {
        return Err("usage: veltrix-sign <raw-32-byte-secret> <manifest.json> <package.exe> <new-signature-file>".into());
    }
    sign(
        Path::new(&args[0]),
        Path::new(&args[1]),
        Path::new(&args[2]),
        Path::new(&args[3]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signs_only_the_matching_installer() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("key");
        let manifest_path = dir.path().join("manifest.json");
        let package = dir.path().join("package.exe");
        let sig = dir.path().join("manifest.sig");
        fs::write(&key_path, [42u8; 32]).unwrap();
        fs::write(&package, b"abc").unwrap();
        fs::write(&manifest_path, br#"{"app_id":"test","version":"1.0.0","channel":"stable","platform":"windows","architecture":"x86_64","size":3,"url":"https://example.com/update.exe","sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","installed_sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}"#).unwrap();
        fs::write(&package, b"bad").unwrap();
        assert!(sign(&key_path, &manifest_path, &package, &sig).is_err());
        assert!(!sig.exists());
        fs::write(&package, b"abc").unwrap();
        sign(&key_path, &manifest_path, &package, &sig).unwrap();
        let policy = veltrix_core::Policy {
            app_id: "test".into(),
            channel: veltrix_core::Channel::Stable,
            current_version: "0.1.0".parse().unwrap(),
            platform: "windows".into(),
            architecture: "x86_64".into(),
            public_key: SigningKey::from_bytes(&[42; 32]).verifying_key().to_bytes(),
            max_package_size: 3,
        };
        assert!(
            policy
                .verify_manifest(&fs::read(manifest_path).unwrap(), &fs::read(&sig).unwrap())
                .is_ok()
        );
        assert!(sign(&key_path, &dir.path().join("manifest.json"), &package, &sig).is_err());
    }
}
