//! One-click unsubscribe tokens (FR-11.5, RFC 8058): `{user_id}.{base64url(HMAC)}`, signed under
//! an HKDF subkey of the master key. No table is needed, and links in sent emails keep working
//! through a master-key rotation because verification tries the previous key too.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use mergetip_crypto::{Keyring, constant_time_eq, hmac_sha256};
use uuid::Uuid;

const PURPOSE: &str = "unsubscribe";

fn mac(key: &[u8], user_id: Uuid) -> String {
    URL_SAFE_NO_PAD.encode(hmac_sha256(
        key,
        format!("unsubscribe:{user_id}").as_bytes(),
    ))
}

/// The token for `user_id`'s unsubscribe links.
pub fn token(keyring: &Keyring, user_id: Uuid) -> String {
    format!(
        "{}.{}",
        user_id.simple(),
        mac(keyring.subkey(PURPOSE).expose(), user_id)
    )
}

/// The user a token unsubscribes, if it is genuine. Constant-time.
pub fn verify(keyring: &Keyring, token: &str) -> Option<Uuid> {
    let (id, signature) = token.trim().split_once('.')?;
    let user_id = Uuid::try_parse(id).ok()?;
    keyring
        .verification_subkeys(PURPOSE)
        .iter()
        .any(|key| constant_time_eq(&mac(key.expose(), user_id), signature))
        .then_some(user_id)
}

/// `{api}/v1/email/unsubscribe?token=…`, the link in emails and in `List-Unsubscribe`.
pub fn url(api_base_url: &str, keyring: &Keyring, user_id: Uuid) -> String {
    format!(
        "{}/v1/email/unsubscribe?token={}",
        api_base_url.trim_end_matches('/'),
        token(keyring, user_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyring(byte: &str, previous: Option<&str>) -> Keyring {
        Keyring::from_hex(&byte.repeat(32), previous.map(|p| p.repeat(32)).as_deref()).unwrap()
    }

    #[test]
    fn tokens_verify_for_their_user_only() {
        let keys = keyring("07", None);
        let user = Uuid::new_v4();
        let token = token(&keys, user);
        assert_eq!(verify(&keys, &token), Some(user));

        let other = Uuid::new_v4();
        let forged = format!("{}.{}", other.simple(), token.split_once('.').unwrap().1);
        assert_eq!(verify(&keys, &forged), None);
        for junk in ["", ".", "abc", "abc.def", &format!("{}.", user.simple())] {
            assert_eq!(verify(&keys, junk), None, "{junk:?}");
        }
        assert_eq!(verify(&keyring("08", None), &token), None);
    }

    #[test]
    fn links_survive_a_master_key_rotation() {
        let user = Uuid::new_v4();
        let sent = token(&keyring("07", None), user);
        assert_eq!(verify(&keyring("08", Some("07")), &sent), Some(user));
    }
}
