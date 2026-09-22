//! The credential Zitadel's setup presents: a JWT it signs for itself.
//!
//! Zitadel's first instance mints an administrator token exactly once, when it
//! is created, and never again. A deployment that loses it cannot administer
//! its own directory, and the only way back is an empty database: the chart
//! trialled that on 2026-09-22 and found it by losing one.
//!
//! A system API user has no such moment. The chart makes an RSA key pair, puts
//! the public half in Zitadel's configuration and keeps the private half in a
//! Secret, and setup signs a short-lived JWT with it whenever it runs. Losing
//! the token means signing another; losing the key means the chart writes a
//! new pair. Nothing is unrecoverable.
//!
//! Signed with `ring` rather than the `rsa` crate, which is why RUSTSEC-2023-0071
//! stays ignorable: that advisory is a timing side channel in `rsa`'s
//! private-key operations, and this repository has none.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};

/// How long a signed credential lasts. Setup takes seconds; an hour is
/// Zitadel's documented example and leaves room for a slow first instance.
const LIFETIME_S: u64 = 3600;

/// The bearer token a system API user presents: `iss` and `sub` its own name,
/// `aud` the instance it is for, signed RS256 with the key the chart holds.
///
/// `audience` is the issuer Zitadel states, not the address setup dials: a
/// deployment reaches Zitadel by its in-cluster service and Zitadel checks the
/// token against its own external domain.
pub fn bearer(user: &str, key_pem: &str, audience: &str, now_s: u64) -> Result<String, String> {
    let key = key_pair(key_pem)?;
    let header = serde_json::json!({"alg": "RS256", "typ": "JWT"});
    let claims = serde_json::json!({
        "iss": user,
        "sub": user,
        "aud": audience,
        "iat": now_s,
        "exp": now_s + LIFETIME_S,
    });

    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );

    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| "the system user's key could not sign".to_string())?;

    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

/// A PEM private key, in either of the two encodings `openssl` writes:
/// `RSA PRIVATE KEY` is PKCS#1, `PRIVATE KEY` is PKCS#8. Which one an
/// administrator has depends on the openssl they ran, and neither is worth
/// making them think about.
fn key_pair(pem: &str) -> Result<RsaKeyPair, String> {
    let (label, der) = decode(pem)?;
    match label.as_str() {
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
        other => return Err(format!("{other} is not an RSA private key")),
    }
    .map_err(|failed| format!("the system user's key could not be read: {failed}"))
}

fn decode(pem: &str) -> Result<(String, Vec<u8>), String> {
    let begin = pem
        .lines()
        .position(|line| line.starts_with("-----BEGIN "))
        .ok_or("the system user's key is not PEM")?;
    let label = pem.lines().nth(begin).unwrap_or_default();
    let label = label
        .trim_start_matches("-----BEGIN ")
        .trim_end_matches("-----")
        .trim()
        .to_string();

    let body: String = pem
        .lines()
        .skip(begin + 1)
        .take_while(|line| !line.starts_with("-----END"))
        .collect();

    let der = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|failed| format!("the system user's key is not base64: {failed}"))?;
    Ok((label, der))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256};

    // Generated for this test and nothing else; it authenticates against no
    // instance that exists.
    const KEY: &str = "\
-----BEGIN RSA PRIVATE KEY-----\n\
MIIEoQIBAAKCAQEA3228nrxYRDspNhgsG+NZaz1S66zI+tQb4kTVYISC9qEWAnSo\n\
ZLlI0fGFnXqxUCGfSd+mJ/Y7MmKxJtVu7Vu9ot3Ff71E8x42L0D8VKWIoJxJy6wA\n\
H3BU7roSiRip09Wn6GfC3qqkYzFK8U2Id7quu9SEwFiBf57VvYtapAhmI/00ezcI\n\
hTsJPi+r78WVw7d7IAoqhsomFVrGUk+QA2Qbn4+wj5H5vh2pzTIef/a6kFLuLQ+H\n\
bMlSW04lZJhYOooYHZxmBBcyDgpYr+DMaRofQT8YYSZ5eqgJ0cZWtg82ibiXxG8j\n\
vREDty0zU2DDEcm3a6IBrMGn51N04g4KgKvRKQIDAQABAoH/Ib8cE4hfIe8i9Qix\n\
oNoLUidGXv0PXiirYtcCuOagNqAeCEDr2PV4tOfF8ViWxNj54Nk8P3ecJAAZbG7Q\n\
Ul7eRbs5brx9AuY385XdMZZ5tx3kB4nzJXcnXOdoj/cpr3/rMdnPlSeGV3Uah1fZ\n\
RObtfXFGm8bCcy7fxGvymilVRBoHk4WvNk3bd72KVqrtjxIu1x9puicSTLSd75ue\n\
+SEmarGbQUWfUCzmhq7jRqua8JzsM/l12gbSeUkdXFVdhL8JI1h5I5DsHjIMhGU6\n\
JVb29QHEVrzrsNwIs2/f3eZD5mH0Z/Jok0pfcFh5z2bACaHQRAkWF8TGfqpW+t3t\n\
o3LxAoGBAPErxzNMoGs5y9044+MRATUuRR6kvGdTlNdk2NfSN/geXXxYJSgXbah9\n\
D+4WO/AR8Utrs+jJnu1/fDO74ncdmjdy/7EBs4wBHJSL+83xdwykGlIbZgj+xO9x\n\
ooE9MZEomJAP8RQYcl6bDq8z6/lIiu0HMYenWD79bC9J9DzvDnVjAoGBAO0qrhtk\n\
0VR+OdKbBXNnryeigSmREUkBCuZEQEYAENcPeKu6b9aayRK80jUYywZ0G+zNEtBX\n\
HRrswwNy/Bt442FutVmGiveXLt5RgStveGjsx7SJB4yKlXn5thff4S+I+USwuuAM\n\
ITHvo36tgrvYXqwsDz/KyS3KS611oQgEFRsDAoGALVcepy1Tx3ThN+D3LvxGbtoZ\n\
Eo7EAOT8yZXjEogqD5Kd3r+vlJ769b81XHx/nj2xUI2aEDy/jUT3c75x8BT3pk8P\n\
dRaty7d1yROcLnaj/BNqA1+1SiGjoqSJeaSoifLI4+SrXSzPa6vZEeVACuixfahp\n\
jmhOteDtEuLjcQU8gaMCgYAvIL8ORH9wWdDlr9Zqc10T9C/UcbZMmn9u+HsJLfQq\n\
uDFTdq3IqGNybMEcufuGIcZ2zN2DNvxaoFe0NMIyN1h/wP8adijhQFKY7PtNBU6Z\n\
EwwwLNaqL9O6NEvh/KQDzSUzaCcKZH6oLKWBg7sp1rohXnP9Si+mAL//DRPdwunq\n\
vwKBgQCeKCXP6lASOJbqDB2VmpbmSYnuzNWztImkysSTgTCrk7xvxwJVuV94Z7Ds\n\
XwD/e29P19xnooBTVwFUF2s/96xSg26VNWsvcW0UfAclQF8Z7e6i2IHzgeNabP8Q\n\
dgUmgxKcXVNlwGXkKhQuRwgPSLAcCtabwgCrE9LopuwFEu8o8A==\n\
-----END RSA PRIVATE KEY-----\n";

    fn parts(token: &str) -> (serde_json::Value, serde_json::Value) {
        let mut split = token.split('.');
        let header = URL_SAFE_NO_PAD.decode(split.next().unwrap()).unwrap();
        let claims = URL_SAFE_NO_PAD.decode(split.next().unwrap()).unwrap();
        (
            serde_json::from_slice(&header).unwrap(),
            serde_json::from_slice(&claims).unwrap(),
        )
    }

    #[test]
    fn it_signs_what_zitadel_verifies() {
        let token = bearer("meridian-setup", KEY, "https://id.example", 1_790_000_000).unwrap();
        let (signing_input, signature) = token.rsplit_once('.').unwrap();

        let key = key_pair(KEY).unwrap();
        let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref());
        public
            .verify(
                signing_input.as_bytes(),
                &URL_SAFE_NO_PAD.decode(signature).unwrap(),
            )
            .expect("the signature verifies against the public half Zitadel is given");
    }

    #[test]
    fn it_names_itself_and_the_instance() {
        let token = bearer("meridian-setup", KEY, "https://id.example", 1_790_000_000).unwrap();
        let (header, claims) = parts(&token);

        assert_eq!(header["alg"], "RS256");
        assert_eq!(claims["iss"], "meridian-setup");
        assert_eq!(claims["sub"], "meridian-setup");
        assert_eq!(claims["aud"], "https://id.example");
        assert_eq!(claims["iat"], 1_790_000_000u64);
        assert_eq!(claims["exp"], 1_790_000_000u64 + 3600);
    }

    #[test]
    fn a_key_that_is_not_a_key_is_refused_by_name() {
        let refusal = bearer(
            "meridian-setup",
            "not a key at all",
            "https://id.example",
            0,
        )
        .expect_err("nothing can be signed with this");
        assert!(refusal.contains("not PEM"), "{refusal}");
    }
}
