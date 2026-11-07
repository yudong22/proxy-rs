//! Argon2id key derivation and the AES-256-GCM seal/open used by encrypted bundles.

use super::*;

/// Derive the key and seal the payload.
///
/// Argon2id's memory cost is what makes a weak passphrase expensive to attack
/// offline; the salt is random per export, so two bundles of the same pool do
/// not share a key.
pub(crate) fn encrypt_payload(
    passphrase: &str,
    payload: &BundlePayload,
) -> Result<(KdfParams, CipherBlob)> {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);

    let key = derive_key(passphrase, &salt, KDF_M_COST, KDF_T_COST, KDF_P_COST)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("加密器初始化失败"))?;
    let plaintext = serde_json::to_vec(payload)?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|_| anyhow!("加密失败"))?;

    Ok((
        KdfParams {
            algo: KDF_ALGO.to_string(),
            m_cost: KDF_M_COST,
            t_cost: KDF_T_COST,
            p_cost: KDF_P_COST,
            salt_b64: B64.encode(salt),
        },
        CipherBlob {
            algo: CIPHER_ALGO.to_string(),
            nonce_b64: B64.encode(nonce),
            ct_b64: B64.encode(ct),
        },
    ))
}

/// Run Argon2id with the parameters stored in the bundle.
fn derive_key(
    passphrase: &str,
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<[u8; KEY_LEN]> {
    let params = Params::new(m_cost, t_cost, p_cost, Some(KEY_LEN))
        .map_err(|e| anyhow!("Argon2 参数无效: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("口令派生失败: {e}"))?;
    Ok(key)
}

/// Open an encrypted envelope.
///
/// A failure here is reported as one message covering wrong passphrase and
/// corrupted file, because AES-GCM cannot tell them apart — claiming to know
/// which it was would be a lie, and telling the user "wrong passphrase" when the
/// file is truncated sends them hunting for a password that was never wrong.
pub(crate) fn decrypt_payload(
    passphrase: &str,
    kdf: &KdfParams,
    blob: &CipherBlob,
) -> Result<BundlePayload> {
    if kdf.algo != KDF_ALGO {
        bail!("不支持的口令派生算法: {}", kdf.algo);
    }
    if blob.algo != CIPHER_ALGO {
        bail!("不支持的加密算法: {}", blob.algo);
    }
    let salt = B64.decode(&kdf.salt_b64).context("口令派生参数损坏")?;
    let nonce = B64.decode(&blob.nonce_b64).context("加密参数损坏")?;
    if nonce.len() != NONCE_LEN {
        bail!("口令错误或文件已损坏");
    }
    let ct = B64.decode(&blob.ct_b64).context("密文损坏")?;

    let key = derive_key(passphrase, &salt, kdf.m_cost, kdf.t_cost, kdf.p_cost)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("加密器初始化失败"))?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), ct.as_slice())
        .map_err(|_| anyhow!("口令错误或文件已损坏"))?;
    serde_json::from_slice(&plaintext).context("解密后的内容不是有效的账号池数据")
}

// ── Decoding ───────────────────────────────────────────────────────────────
