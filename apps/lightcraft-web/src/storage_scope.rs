//! A browser library's private namespace, shared by the page and its render workers.

pub struct StorageScope(String);

impl StorageScope {
    /// The host supplies the lowercase SHA-256 scope identifier for this account library.
    pub fn parse(value: &str) -> Option<Self> {
        (value.len() == 64 && value.bytes().all(|ch| ch.is_ascii_digit() || (b'a'..=b'f').contains(&ch))).then(|| Self(value.to_string()))
    }

    pub fn name(&self) -> String {
        format!("lightcraft-webos-{}", self.0)
    }

    pub fn worker_store(&self, kind: &str) -> String {
        format!("{kind}:{}", self.0)
    }

    pub fn parse_worker_store(value: &str) -> Result<(&str, Option<Self>), &'static str> {
        let (kind, scope) = match value.split_once(':') {
            Some((kind, scope)) => (kind, Some(Self::parse(scope).ok_or("invalid library scope")?)),
            None => (value, None),
        };
        match kind {
            "opfs" | "idb" => Ok((kind, scope)),
            "memory" if scope.is_none() => Ok((kind, None)),
            _ => Err("invalid browser storage backend"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_distinct_and_can_be_recovered_by_a_worker() {
        let first = StorageScope::parse(&"a".repeat(64)).unwrap();
        let second = StorageScope::parse(&"b".repeat(64)).unwrap();
        assert_ne!(first.name(), second.name());
        let spec = first.worker_store("opfs");
        let (kind, scope) = StorageScope::parse_worker_store(&spec).unwrap();
        assert_eq!(kind, "opfs");
        assert_eq!(scope.unwrap().name(), first.name());
        assert!(matches!(StorageScope::parse_worker_store("idb"), Ok(("idb", None))));
    }

    #[test]
    fn invalid_worker_scopes_do_not_downgrade_to_legacy_storage() {
        for spec in ["opfs:", "idb:../../legacy", "idb:bad", "unknown"] {
            assert!(StorageScope::parse_worker_store(spec).is_err());
        }
        assert!(StorageScope::parse_worker_store(&format!("memory:{}", "0".repeat(64))).is_err());
    }
}
