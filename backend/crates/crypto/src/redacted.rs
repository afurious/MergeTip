use std::fmt;

use serde::{Deserialize, Deserializer};
use zeroize::Zeroize;

/// Holds a secret so it cannot leak through `Debug`, `Display` or serialisation, and wipes it
/// from memory on drop.
///
/// There is deliberately no `Serialize` impl: a secret has to be exposed explicitly with
/// [`Redacted::expose`] before it can be written anywhere.
pub struct Redacted<T: Zeroize>(T);

impl<T: Zeroize> Redacted<T> {
    pub fn new(secret: T) -> Self {
        Self(secret)
    }

    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T: Zeroize> Drop for Redacted<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<T: Zeroize + Clone> Clone for Redacted<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Zeroize + PartialEq> PartialEq for Redacted<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: Zeroize> From<T> for Redacted<T> {
    fn from(secret: T) -> Self {
        Self(secret)
    }
}

impl<T: Zeroize> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T: Zeroize> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<'de, T: Zeroize + Deserialize<'de>> Deserialize<'de> for Redacted<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_never_shows_the_secret() {
        let secret = Redacted::new(String::from("nostr+walletconnect://secret"));
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
        assert_eq!(format!("{secret}"), "[REDACTED]");
        assert_eq!(format!("{:?}", Some(&secret)), "Some([REDACTED])");
        assert_eq!(secret.expose(), "nostr+walletconnect://secret");
    }

    #[test]
    fn deserializes_transparently() {
        #[derive(Deserialize)]
        struct Body {
            nwc_uri: Redacted<String>,
        }
        let body: Body = serde_json::from_str(r#"{"nwc_uri":"abc"}"#).unwrap();
        assert_eq!(body.nwc_uri.expose(), "abc");
    }
}
