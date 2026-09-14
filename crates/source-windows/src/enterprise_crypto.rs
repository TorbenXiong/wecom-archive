//! Windows CNG-backed hybrid encryption for offline enterprise packages.
//! The collector receives only the RSA public blob. The server retains the
//! private blob and unwraps a fresh AES-256 key for every package.

#[cfg(windows)]
mod platform {
    use sha2::{Digest, Sha256};
    use std::ffi::{OsStr, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    type NtStatus = i32;
    type AlgHandle = *mut c_void;
    type KeyHandle = *mut c_void;

    const SUCCESS: NtStatus = 0;
    const USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    const PAD_OAEP: u32 = 0x0000_0004;
    const PAD_PSS: u32 = 0x0000_0008;

    #[repr(C)]
    struct OaepPaddingInfo {
        algorithm_id: *const u16,
        label: *mut u8,
        label_len: u32,
    }

    #[repr(C)]
    struct PssPaddingInfo {
        algorithm_id: *const u16,
        salt_len: u32,
    }

    #[repr(C)]
    struct AuthenticatedCipherModeInfo {
        size: u32,
        version: u32,
        nonce: *mut u8,
        nonce_len: u32,
        auth_data: *mut u8,
        auth_data_len: u32,
        tag: *mut u8,
        tag_len: u32,
        mac_context: *mut u8,
        mac_context_len: u32,
        aad_len: u32,
        data_len: u64,
        flags: u32,
    }

    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptOpenAlgorithmProvider(
            handle: *mut AlgHandle,
            algorithm: *const u16,
            implementation: *const u16,
            flags: u32,
        ) -> NtStatus;
        fn BCryptCloseAlgorithmProvider(handle: AlgHandle, flags: u32) -> NtStatus;
        fn BCryptSetProperty(
            handle: *mut c_void,
            property: *const u16,
            input: *const u8,
            input_len: u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptGetProperty(
            handle: *mut c_void,
            property: *const u16,
            output: *mut u8,
            output_len: u32,
            result_len: *mut u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptGenRandom(
            handle: AlgHandle,
            output: *mut u8,
            output_len: u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptGenerateKeyPair(
            algorithm: AlgHandle,
            key: *mut KeyHandle,
            bits: u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptFinalizeKeyPair(key: KeyHandle, flags: u32) -> NtStatus;
        fn BCryptExportKey(
            key: KeyHandle,
            export_key: KeyHandle,
            blob_type: *const u16,
            output: *mut u8,
            output_len: u32,
            result_len: *mut u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptImportKeyPair(
            algorithm: AlgHandle,
            import_key: KeyHandle,
            blob_type: *const u16,
            key: *mut KeyHandle,
            input: *const u8,
            input_len: u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptGenerateSymmetricKey(
            algorithm: AlgHandle,
            key: *mut KeyHandle,
            key_object: *mut u8,
            key_object_len: u32,
            secret: *const u8,
            secret_len: u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptEncrypt(
            key: KeyHandle,
            input: *const u8,
            input_len: u32,
            padding: *mut c_void,
            iv: *mut u8,
            iv_len: u32,
            output: *mut u8,
            output_len: u32,
            result_len: *mut u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptDecrypt(
            key: KeyHandle,
            input: *const u8,
            input_len: u32,
            padding: *mut c_void,
            iv: *mut u8,
            iv_len: u32,
            output: *mut u8,
            output_len: u32,
            result_len: *mut u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptSignHash(
            key: KeyHandle,
            padding: *mut c_void,
            hash: *const u8,
            hash_len: u32,
            signature: *mut u8,
            signature_len: u32,
            result_len: *mut u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptVerifySignature(
            key: KeyHandle,
            padding: *mut c_void,
            hash: *const u8,
            hash_len: u32,
            signature: *const u8,
            signature_len: u32,
            flags: u32,
        ) -> NtStatus;
        fn BCryptDestroyKey(key: KeyHandle) -> NtStatus;
    }

    struct Algorithm(AlgHandle);
    impl Drop for Algorithm {
        fn drop(&mut self) {
            unsafe {
                BCryptCloseAlgorithmProvider(self.0, 0);
            }
        }
    }
    struct Key(KeyHandle);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                BCryptDestroyKey(self.0);
            }
        }
    }

    #[derive(Debug)]
    pub struct HybridCiphertext {
        pub wrapped_dek: Vec<u8>,
        pub nonce: [u8; 12],
        pub ciphertext: Vec<u8>,
        pub tag: [u8; 16],
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    fn check(status: NtStatus) -> Result<(), String> {
        if status >= SUCCESS {
            Ok(())
        } else {
            Err(format!("CNG status 0x{:08x}", status as u32))
        }
    }

    fn open_algorithm(name: &str) -> Result<Algorithm, String> {
        let mut handle = null_mut();
        check(unsafe { BCryptOpenAlgorithmProvider(&mut handle, wide(name).as_ptr(), null(), 0) })?;
        Ok(Algorithm(handle))
    }

    pub fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
        let mut bytes = [0_u8; N];
        check(unsafe {
            BCryptGenRandom(
                null_mut(),
                bytes.as_mut_ptr(),
                N as u32,
                USE_SYSTEM_PREFERRED_RNG,
            )
        })?;
        Ok(bytes)
    }

    pub fn generate_rsa_key_pair() -> Result<(Vec<u8>, Vec<u8>), String> {
        let algorithm = open_algorithm("RSA")?;
        let mut key = null_mut();
        check(unsafe { BCryptGenerateKeyPair(algorithm.0, &mut key, 3072, 0) })?;
        let key = Key(key);
        check(unsafe { BCryptFinalizeKeyPair(key.0, 0) })?;
        Ok((
            export_key(key.0, "RSAPUBLICBLOB")?,
            export_key(key.0, "RSAFULLPRIVATEBLOB")?,
        ))
    }

    fn export_key(key: KeyHandle, blob_type: &str) -> Result<Vec<u8>, String> {
        let blob_type = wide(blob_type);
        let mut size = 0;
        check(unsafe {
            BCryptExportKey(
                key,
                null_mut(),
                blob_type.as_ptr(),
                null_mut(),
                0,
                &mut size,
                0,
            )
        })?;
        let mut output = vec![0_u8; size as usize];
        check(unsafe {
            BCryptExportKey(
                key,
                null_mut(),
                blob_type.as_ptr(),
                output.as_mut_ptr(),
                size,
                &mut size,
                0,
            )
        })?;
        output.truncate(size as usize);
        Ok(output)
    }

    fn import_rsa(blob: &[u8], blob_type: &str) -> Result<(Algorithm, Key), String> {
        let algorithm = open_algorithm("RSA")?;
        let mut key = null_mut();
        check(unsafe {
            BCryptImportKeyPair(
                algorithm.0,
                null_mut(),
                wide(blob_type).as_ptr(),
                &mut key,
                blob.as_ptr(),
                blob.len() as u32,
                0,
            )
        })?;
        Ok((algorithm, Key(key)))
    }

    fn rsa_encrypt(public_key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let (_algorithm, key) = import_rsa(public_key, "RSAPUBLICBLOB")?;
        let algorithm_id = wide("SHA256");
        let mut padding = OaepPaddingInfo {
            algorithm_id: algorithm_id.as_ptr(),
            label: null_mut(),
            label_len: 0,
        };
        rsa_crypt(key.0, plaintext, &mut padding, true)
    }

    fn rsa_decrypt(private_key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
        let (_algorithm, key) = import_rsa(private_key, "RSAFULLPRIVATEBLOB")?;
        let algorithm_id = wide("SHA256");
        let mut padding = OaepPaddingInfo {
            algorithm_id: algorithm_id.as_ptr(),
            label: null_mut(),
            label_len: 0,
        };
        rsa_crypt(key.0, ciphertext, &mut padding, false)
    }

    fn rsa_crypt(
        key: KeyHandle,
        input: &[u8],
        padding: &mut OaepPaddingInfo,
        encrypt: bool,
    ) -> Result<Vec<u8>, String> {
        let mut size = 0;
        let mut call = |output: *mut u8, output_len, result: *mut u32| unsafe {
            if encrypt {
                BCryptEncrypt(
                    key,
                    input.as_ptr(),
                    input.len() as u32,
                    padding as *mut _ as _,
                    null_mut(),
                    0,
                    output,
                    output_len,
                    result,
                    PAD_OAEP,
                )
            } else {
                BCryptDecrypt(
                    key,
                    input.as_ptr(),
                    input.len() as u32,
                    padding as *mut _ as _,
                    null_mut(),
                    0,
                    output,
                    output_len,
                    result,
                    PAD_OAEP,
                )
            }
        };
        check(call(null_mut(), 0, &mut size))?;
        let mut output = vec![0_u8; size as usize];
        check(call(output.as_mut_ptr(), size, &mut size))?;
        output.truncate(size as usize);
        Ok(output)
    }

    pub fn sign(private_key: &[u8], message: &[u8]) -> Result<Vec<u8>, String> {
        let (_algorithm, key) = import_rsa(private_key, "RSAFULLPRIVATEBLOB")?;
        let algorithm_id = wide("SHA256");
        let mut padding = PssPaddingInfo {
            algorithm_id: algorithm_id.as_ptr(),
            salt_len: 32,
        };
        let digest = Sha256::digest(message);
        let mut size = 0;
        check(unsafe {
            BCryptSignHash(
                key.0,
                &mut padding as *mut _ as _,
                digest.as_ptr(),
                digest.len() as u32,
                null_mut(),
                0,
                &mut size,
                PAD_PSS,
            )
        })?;
        let mut signature = vec![0_u8; size as usize];
        check(unsafe {
            BCryptSignHash(
                key.0,
                &mut padding as *mut _ as _,
                digest.as_ptr(),
                digest.len() as u32,
                signature.as_mut_ptr(),
                size,
                &mut size,
                PAD_PSS,
            )
        })?;
        signature.truncate(size as usize);
        Ok(signature)
    }

    pub fn verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<(), String> {
        let (_algorithm, key) = import_rsa(public_key, "RSAPUBLICBLOB")?;
        let algorithm_id = wide("SHA256");
        let mut padding = PssPaddingInfo {
            algorithm_id: algorithm_id.as_ptr(),
            salt_len: 32,
        };
        let digest = Sha256::digest(message);
        check(unsafe {
            BCryptVerifySignature(
                key.0,
                &mut padding as *mut _ as _,
                digest.as_ptr(),
                digest.len() as u32,
                signature.as_ptr(),
                signature.len() as u32,
                PAD_PSS,
            )
        })
    }

    fn aes_key(dek: &[u8]) -> Result<(Algorithm, Vec<u8>, Key), String> {
        let algorithm = open_algorithm("AES")?;
        let chaining_mode = wide("ChainingModeGCM");
        check(unsafe {
            BCryptSetProperty(
                algorithm.0,
                wide("ChainingMode").as_ptr(),
                chaining_mode.as_ptr() as *const u8,
                (chaining_mode.len() * 2) as u32,
                0,
            )
        })?;
        let mut object_len = 0_u32;
        let mut copied = 0_u32;
        check(unsafe {
            BCryptGetProperty(
                algorithm.0,
                wide("ObjectLength").as_ptr(),
                &mut object_len as *mut _ as _,
                4,
                &mut copied,
                0,
            )
        })?;
        let mut object = vec![0_u8; object_len as usize];
        let mut key = null_mut();
        check(unsafe {
            BCryptGenerateSymmetricKey(
                algorithm.0,
                &mut key,
                object.as_mut_ptr(),
                object_len,
                dek.as_ptr(),
                dek.len() as u32,
                0,
            )
        })?;
        Ok((algorithm, object, Key(key)))
    }

    fn auth_info(nonce: &mut [u8], tag: &mut [u8]) -> AuthenticatedCipherModeInfo {
        AuthenticatedCipherModeInfo {
            size: std::mem::size_of::<AuthenticatedCipherModeInfo>() as u32,
            version: 1,
            nonce: nonce.as_mut_ptr(),
            nonce_len: nonce.len() as u32,
            auth_data: null_mut(),
            auth_data_len: 0,
            tag: tag.as_mut_ptr(),
            tag_len: tag.len() as u32,
            mac_context: null_mut(),
            mac_context_len: 0,
            aad_len: 0,
            data_len: 0,
            flags: 0,
        }
    }

    fn aes_encrypt(
        dek: &[u8],
        plaintext: &[u8],
        nonce: &mut [u8; 12],
    ) -> Result<(Vec<u8>, [u8; 16]), String> {
        let (_algorithm, _object, key) = aes_key(dek)?;
        let mut tag = [0_u8; 16];
        let mut info = auth_info(nonce, &mut tag);
        let mut ciphertext = vec![0_u8; plaintext.len()];
        let mut written = 0;
        check(unsafe {
            BCryptEncrypt(
                key.0,
                plaintext.as_ptr(),
                plaintext.len() as u32,
                &mut info as *mut _ as _,
                null_mut(),
                0,
                ciphertext.as_mut_ptr(),
                ciphertext.len() as u32,
                &mut written,
                0,
            )
        })?;
        ciphertext.truncate(written as usize);
        Ok((ciphertext, tag))
    }

    fn aes_decrypt(
        dek: &[u8],
        ciphertext: &[u8],
        nonce: &mut [u8; 12],
        tag: &mut [u8; 16],
    ) -> Result<Vec<u8>, String> {
        let (_algorithm, _object, key) = aes_key(dek)?;
        let mut info = auth_info(nonce, tag);
        let mut plaintext = vec![0_u8; ciphertext.len()];
        let mut written = 0;
        check(unsafe {
            BCryptDecrypt(
                key.0,
                ciphertext.as_ptr(),
                ciphertext.len() as u32,
                &mut info as *mut _ as _,
                null_mut(),
                0,
                plaintext.as_mut_ptr(),
                plaintext.len() as u32,
                &mut written,
                0,
            )
        })?;
        plaintext.truncate(written as usize);
        Ok(plaintext)
    }

    pub fn encrypt(public_key: &[u8], plaintext: &[u8]) -> Result<HybridCiphertext, String> {
        let dek = random_bytes::<32>()?;
        let mut nonce = random_bytes::<12>()?;
        let (ciphertext, tag) = aes_encrypt(&dek, plaintext, &mut nonce)?;
        let wrapped_dek = rsa_encrypt(public_key, &dek)?;
        Ok(HybridCiphertext {
            wrapped_dek,
            nonce,
            ciphertext,
            tag,
        })
    }

    pub fn decrypt(
        private_key: &[u8],
        wrapped_dek: &[u8],
        nonce: &[u8],
        ciphertext: &[u8],
        tag: &[u8],
    ) -> Result<Vec<u8>, String> {
        let dek = rsa_decrypt(private_key, wrapped_dek)?;
        if dek.len() != 32 || nonce.len() != 12 || tag.len() != 16 {
            return Err("invalid enterprise cipher parameters".into());
        }
        let mut nonce_array = [0_u8; 12];
        nonce_array.copy_from_slice(nonce);
        let mut tag_array = [0_u8; 16];
        tag_array.copy_from_slice(tag);
        aes_decrypt(&dek, ciphertext, &mut nonce_array, &mut tag_array)
    }
}

#[cfg(windows)]
pub use platform::*;

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn cng_round_trip_uses_public_key_for_encryption() {
        let (public_key, private_key) = generate_rsa_key_pair().expect("generate RSA key pair");
        let plaintext = b"enterprise fixture";
        let encrypted = encrypt(&public_key, plaintext).expect("encrypt fixture");
        let decrypted = decrypt(
            &private_key,
            &encrypted.wrapped_dek,
            &encrypted.nonce,
            &encrypted.ciphertext,
            &encrypted.tag,
        )
        .expect("decrypt fixture");
        assert_eq!(decrypted, plaintext);
        let mut tampered = encrypted.tag;
        tampered[0] ^= 1;
        assert!(
            decrypt(
                &private_key,
                &encrypted.wrapped_dek,
                &encrypted.nonce,
                &encrypted.ciphertext,
                &tampered,
            )
            .is_err()
        );
        let signature = sign(&private_key, plaintext).expect("sign fixture");
        verify(&public_key, plaintext, &signature).expect("verify fixture");
        assert!(verify(&public_key, b"changed", &signature).is_err());
    }
}

#[cfg(not(windows))]
mod unsupported {
    #[derive(Debug)]
    pub struct HybridCiphertext {
        pub wrapped_dek: Vec<u8>,
        pub nonce: [u8; 12],
        pub ciphertext: Vec<u8>,
        pub tag: [u8; 16],
    }
    pub fn generate_rsa_key_pair() -> Result<(Vec<u8>, Vec<u8>), String> {
        Err("enterprise encryption requires Windows CNG".into())
    }
    pub fn encrypt(_: &[u8], _: &[u8]) -> Result<HybridCiphertext, String> {
        Err("enterprise encryption requires Windows CNG".into())
    }
    pub fn decrypt(_: &[u8], _: &[u8], _: &[u8], _: &[u8], _: &[u8]) -> Result<Vec<u8>, String> {
        Err("enterprise encryption requires Windows CNG".into())
    }
    pub fn sign(_: &[u8], _: &[u8]) -> Result<Vec<u8>, String> {
        Err("enterprise signing requires Windows CNG".into())
    }
    pub fn verify(_: &[u8], _: &[u8], _: &[u8]) -> Result<(), String> {
        Err("enterprise signing requires Windows CNG".into())
    }
}

#[cfg(not(windows))]
pub use unsupported::*;
