//! What this Mac keeps about each Mac it controls, apart from trusting it: the name the user calls
//! it by, where it was last reached on the local network, and how to connect to it by name (on the
//! local network, over the internet, or whichever answers first). Its address over the internet is
//! kept with its access key ([`crate::internet::InternetHosts`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use transport::identity::{Fingerprint, write_private};

use crate::{from_hex, hex};

/// The longest name kept, in characters.
const MAX_ALIAS: usize = 64;
/// The longest address kept.
const MAX_ADDRESS_LEN: usize = 255;

/// How to connect to a paired Mac by name (`lankvm:<fingerprint>`). A typed address goes where it
/// says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// Every way this Mac knows, all at once: its address on the local network, its addresses on
    /// the internet and its LanKVM server. The local network wins when it answers too.
    #[default]
    Auto,
    /// Only at its address on the local network.
    Local,
    /// Only over the internet: at its addresses there and through its LanKVM server. A session
    /// through the server's relay stays on it, even when the Mac turns out to be on this network.
    Internet,
}

impl Via {
    /// "auto", "local" or "internet".
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "auto" => Some(Self::Auto),
            "local" => Some(Self::Local),
            "internet" => Some(Self::Internet),
            _ => None,
        }
    }
}

/// Stored in `address-book.json`, keyed by fingerprint (hex). A Mac with nothing to keep (its own
/// name, no local address, connected to automatically) has no entry.
pub(crate) struct AddressBook {
    path: PathBuf,
    hosts: BTreeMap<Fingerprint, Entry>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Entry {
    /// The name the user calls it by; "" for the one it gave itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    alias: String,
    /// Where this Mac last reached it on the local network ("192.168.1.31:47800"); "" if never.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    local: String,
    /// One this LanKVM doesn't know (from a newer one) is automatic, rather than costing the whole
    /// book.
    #[serde(default, rename = "connection", deserialize_with = "lenient_via")]
    via: Via,
}

fn lenient_via<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Via, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value.as_str().and_then(Via::parse).unwrap_or_default())
}

impl Entry {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl AddressBook {
    pub(crate) fn load(path: &Path) -> Self {
        let stored: BTreeMap<String, Entry> =
            std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let hosts = stored.into_iter().filter_map(|(fp, entry)| Some((from_hex(&fp)?, entry))).collect();
        Self { path: path.to_path_buf(), hosts }
    }

    fn save(&self) {
        let stored: BTreeMap<String, &Entry> = self.hosts.iter().map(|(fp, entry)| (hex(fp), entry)).collect();
        let result = serde_json::to_vec_pretty(&stored).map_err(anyhow::Error::from).and_then(|bytes| write_private(&self.path, &bytes));
        if let Err(e) = result {
            tracing::warn!("save {}: {e:#}", self.path.display());
        }
    }

    /// Changes `host`'s entry with `change`, and saves it if that changed anything (an entry left
    /// with nothing in it goes). True if it did.
    fn change(&mut self, host: &Fingerprint, change: impl FnOnce(&mut Entry)) -> bool {
        let before = self.hosts.get(host).cloned().unwrap_or_default();
        let mut after = before.clone();
        change(&mut after);
        if after == before {
            return false;
        }
        if after.is_empty() {
            self.hosts.remove(host);
        } else {
            self.hosts.insert(*host, after);
        }
        self.save();
        true
    }

    /// The name the user calls `host` by, if they gave it one.
    pub(crate) fn alias(&self, host: &Fingerprint) -> Option<String> {
        self.hosts.get(host).map(|e| e.alias.clone()).filter(|alias| !alias.is_empty())
    }

    /// Calls `host` `alias` from now on ("" for the name it gave itself): on one line, trimmed, and
    /// cut to [`MAX_ALIAS`] characters. True if that changed anything.
    pub(crate) fn set_alias(&mut self, host: &Fingerprint, alias: &str) -> bool {
        let alias: String = alias.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_ALIAS).collect();
        self.change(host, |e| e.alias = alias.trim_end().to_string())
    }

    /// Where `host` was last reached on the local network.
    pub(crate) fn local(&self, host: &Fingerprint) -> Option<String> {
        self.hosts.get(host).map(|e| e.local.clone()).filter(|local| !local.is_empty())
    }

    /// `host` was reached at `address` on the local network. True if that is news.
    pub(crate) fn set_local(&mut self, host: &Fingerprint, address: &str) -> bool {
        let address = address.trim();
        if address.is_empty() || address.len() > MAX_ADDRESS_LEN {
            return false;
        }
        self.change(host, |e| e.local = address.to_string())
    }

    /// How to connect to `host` by name.
    pub(crate) fn via(&self, host: &Fingerprint) -> Via {
        self.hosts.get(host).map(|e| e.via).unwrap_or_default()
    }

    /// True if that changed anything.
    pub(crate) fn set_via(&mut self, host: &Fingerprint, via: Via) -> bool {
        self.change(host, |e| e.via = via)
    }

    /// Forgets `host` (Forget Device).
    pub(crate) fn remove(&mut self, host: &Fingerprint) {
        if self.hosts.remove(host).is_some() {
            self.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("lankvm-book-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn book(&self) -> AddressBook {
            AddressBook::load(&self.0.join("address-book.json"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fp(n: u8) -> Fingerprint {
        [n; 32]
    }

    #[test]
    fn entries_round_trip() {
        let dir = TempDir::new("round-trip");
        let mut book = dir.book();
        assert_eq!((book.alias(&fp(1)), book.local(&fp(1)), book.via(&fp(1))), (None, None, Via::Auto));
        assert!(book.set_alias(&fp(1), "Office"));
        assert!(!book.set_alias(&fp(1), "Office"), "nothing new");
        assert!(book.set_local(&fp(1), "192.168.1.31:47800"));
        assert!(!book.set_local(&fp(1), " 192.168.1.31:47800 "), "nothing new");
        assert!(book.set_via(&fp(1), Via::Internet));
        assert!(book.set_via(&fp(2), Via::Local));

        let loaded = dir.book();
        assert_eq!(loaded.alias(&fp(1)).as_deref(), Some("Office"));
        assert_eq!(loaded.local(&fp(1)).as_deref(), Some("192.168.1.31:47800"));
        assert_eq!((loaded.via(&fp(1)), loaded.via(&fp(2))), (Via::Internet, Via::Local));
        assert_eq!(loaded.alias(&fp(2)), None);
        let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.0.join("address-book.json")).unwrap()).unwrap();
        assert_eq!(stored[hex(&fp(1))], serde_json::json!({"alias": "Office", "local": "192.168.1.31:47800", "connection": "internet"}));
        assert_eq!(stored[hex(&fp(2))], serde_json::json!({"connection": "local"}));
    }

    #[test]
    fn aliases_are_one_short_line() {
        let dir = TempDir::new("alias");
        let mut book = dir.book();
        book.set_alias(&fp(1), "  Mac\tmini \n upstairs ");
        assert_eq!(book.alias(&fp(1)).as_deref(), Some("Mac mini upstairs"));
        book.set_alias(&fp(1), &"é".repeat(100));
        assert_eq!(book.alias(&fp(1)).map(|a| a.chars().count()), Some(MAX_ALIAS));
        assert!(book.set_alias(&fp(1), "   "), "blank: its own name again");
        assert_eq!(book.alias(&fp(1)), None);
    }

    #[test]
    fn a_mac_with_nothing_to_keep_has_no_entry() {
        let dir = TempDir::new("empty");
        let mut book = dir.book();
        book.set_alias(&fp(1), "Office");
        book.set_via(&fp(1), Via::Local);
        book.set_alias(&fp(1), "");
        book.set_via(&fp(1), Via::Auto);
        assert!(dir.book().hosts.is_empty());
        assert!(!book.set_local(&fp(1), ""), "no address");
        assert!(!book.set_local(&fp(1), &"a".repeat(MAX_ADDRESS_LEN + 1)), "too long");

        book.set_local(&fp(2), "192.168.1.31:47800");
        book.remove(&fp(2));
        assert!(dir.book().hosts.is_empty());
    }

    #[test]
    fn a_damaged_book_is_empty() {
        let dir = TempDir::new("damaged");
        std::fs::write(dir.0.join("address-book.json"), b"{not json").unwrap();
        assert!(dir.book().hosts.is_empty());
        let text = format!(r#"{{"nothex": {{"alias": "x"}}, "{}": {{"alias": "Office", "connection": "carrier pigeon"}}}}"#, hex(&fp(1)));
        std::fs::write(dir.0.join("address-book.json"), text).unwrap();
        let book = dir.book();
        assert_eq!(book.hosts.len(), 1);
        assert_eq!((book.alias(&fp(1)).as_deref(), book.via(&fp(1))), (Some("Office"), Via::Auto), "an unknown way is automatic");
    }

    #[test]
    fn connection_types_parse() {
        assert_eq!(Via::parse("auto"), Some(Via::Auto));
        assert_eq!(Via::parse(" local"), Some(Via::Local));
        assert_eq!(Via::parse("internet"), Some(Via::Internet));
        assert_eq!(Via::parse("global"), None);
    }
}
