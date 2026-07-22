//! Contact-name resolution from the macOS Contacts (AddressBook) store.
//!
//! Read-only, like the Messages source: names are looked up locally and
//! stored only in the private index. The store is a directory containing
//! `AddressBook-v22.abcddb` plus one such database per account under
//! `Sources/<uuid>/`; all are scanned and merged (first name wins).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::handles::normalize_handle;

#[derive(Debug, thiserror::Error)]
pub enum ContactsError {
    #[error("no Contacts database found under {0}")]
    NotFound(PathBuf),
    #[error("could not open Contacts database {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("Contacts query failed in {path}: {source}")]
    Query {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
}

/// (first, last, organization, phone-or-email) as read from AddressBook.
type NameRow = (Option<String>, Option<String>, Option<String>, String);

/// Handle → display-name lookup built from the AddressBook databases.
#[derive(Debug, Default)]
pub struct ContactBook {
    /// Keys are normalized handles plus, for phones, a last-10-digits
    /// fallback so `+19165550100` still matches `(916) 555-0100`.
    map: HashMap<String, String>,
    databases: usize,
}

impl ContactBook {
    /// Scan `root` (the AddressBook directory) and every account database
    /// under `Sources/`. Errors only when nothing at all could be read.
    pub fn load(root: &Path) -> Result<ContactBook, ContactsError> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        let direct = root.join("AddressBook-v22.abcddb");
        if direct.exists() {
            candidates.push(direct);
        }
        if let Ok(sources) = std::fs::read_dir(root.join("Sources")) {
            for entry in sources.flatten() {
                let db = entry.path().join("AddressBook-v22.abcddb");
                if db.exists() {
                    candidates.push(db);
                }
            }
        }
        if candidates.is_empty() {
            return Err(ContactsError::NotFound(root.to_path_buf()));
        }

        let mut book = ContactBook::default();
        let mut last_err = None;
        for path in candidates {
            match book.load_db(&path) {
                Ok(()) => book.databases += 1,
                Err(e) => last_err = Some(e),
            }
        }
        if book.databases == 0 {
            return Err(last_err.expect("at least one candidate was attempted"));
        }
        Ok(book)
    }

    fn load_db(&mut self, path: &Path) -> Result<(), ContactsError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| ContactsError::Open {
            path: path.to_path_buf(),
            source: e,
        })?;
        conn.busy_timeout(Duration::from_secs(3))
            .map_err(|e| ContactsError::Open {
                path: path.to_path_buf(),
                source: e,
            })?;
        let query = |sql: &str| -> Result<Vec<NameRow>, rusqlite::Error> {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        };

        let phones = query(
            "SELECT r.ZFIRSTNAME, r.ZLASTNAME, r.ZORGANIZATION, p.ZFULLNUMBER
             FROM ZABCDPHONENUMBER p JOIN ZABCDRECORD r ON r.Z_PK = p.ZOWNER
             WHERE p.ZFULLNUMBER IS NOT NULL",
        )
        .map_err(|e| ContactsError::Query {
            path: path.to_path_buf(),
            source: e,
        })?;
        for (first, last, org, number) in phones {
            if let Some(name) = display_name(first, last, org) {
                for key in phone_keys(&number) {
                    self.map.entry(key).or_insert_with(|| name.clone());
                }
            }
        }

        let emails = query(
            "SELECT r.ZFIRSTNAME, r.ZLASTNAME, r.ZORGANIZATION, e.ZADDRESS
             FROM ZABCDEMAILADDRESS e JOIN ZABCDRECORD r ON r.Z_PK = e.ZOWNER
             WHERE e.ZADDRESS IS NOT NULL",
        )
        .map_err(|e| ContactsError::Query {
            path: path.to_path_buf(),
            source: e,
        })?;
        for (first, last, org, address) in emails {
            if let Some(name) = display_name(first, last, org) {
                self.map
                    .entry(normalize_handle(&address))
                    .or_insert_with(|| name.clone());
            }
        }
        Ok(())
    }

    /// Resolve a Messages handle (E.164 phone or email) to a contact name.
    pub fn resolve(&self, handle: &str) -> Option<&str> {
        for key in phone_keys(handle) {
            if let Some(name) = self.map.get(&key) {
                return Some(name);
            }
        }
        None
    }

    pub fn names(&self) -> usize {
        self.map.len()
    }

    pub fn databases(&self) -> usize {
        self.databases
    }
}

fn display_name(
    first: Option<String>,
    last: Option<String>,
    org: Option<String>,
) -> Option<String> {
    let first = first.map(|s| s.trim().to_string()).unwrap_or_default();
    let last = last.map(|s| s.trim().to_string()).unwrap_or_default();
    let name = match (first.is_empty(), last.is_empty()) {
        (false, false) => format!("{first} {last}"),
        (false, true) => first,
        (true, false) => last,
        (true, true) => org.map(|s| s.trim().to_string()).unwrap_or_default(),
    };
    (!name.is_empty()).then_some(name)
}

/// Lookup keys for one raw handle or stored contact number, most specific
/// first. Contacts store numbers formatted ("(916) 555-0100") while
/// Messages uses E.164 ("+19165550100"); comparing on digits with a
/// last-10 fallback bridges the difference in country-code prefixes.
fn phone_keys(raw: &str) -> Vec<String> {
    let normalized = normalize_handle(raw);
    if normalized.contains('@') || normalized.is_empty() {
        return vec![normalized];
    }
    let digits: String = normalized.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return vec![normalized];
    }
    let mut keys = vec![digits.clone()];
    if digits.len() > 10 {
        keys.push(digits[digits.len() - 10..].to_string());
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;

    type ContactRow<'a> = (&'a str, &'a str, &'a str, &'a [&'a str], &'a [&'a str]);

    fn build_db(path: &Path, rows: &[ContactRow]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZABCDRECORD (
               Z_PK INTEGER PRIMARY KEY, ZFIRSTNAME TEXT, ZLASTNAME TEXT, ZORGANIZATION TEXT
             );
             CREATE TABLE ZABCDPHONENUMBER (
               Z_PK INTEGER PRIMARY KEY, ZOWNER INTEGER, ZFULLNUMBER TEXT
             );
             CREATE TABLE ZABCDEMAILADDRESS (
               Z_PK INTEGER PRIMARY KEY, ZOWNER INTEGER, ZADDRESS TEXT
             );",
        )
        .unwrap();
        for (first, last, org, phones, emails) in rows {
            conn.execute(
                "INSERT INTO ZABCDRECORD (ZFIRSTNAME, ZLASTNAME, ZORGANIZATION)
                 VALUES (NULLIF(?1, ''), NULLIF(?2, ''), NULLIF(?3, ''))",
                params![first, last, org],
            )
            .unwrap();
            let owner = conn.last_insert_rowid();
            for p in *phones {
                conn.execute(
                    "INSERT INTO ZABCDPHONENUMBER (ZOWNER, ZFULLNUMBER) VALUES (?1, ?2)",
                    params![owner, p],
                )
                .unwrap();
            }
            for e in *emails {
                conn.execute(
                    "INSERT INTO ZABCDEMAILADDRESS (ZOWNER, ZADDRESS) VALUES (?1, ?2)",
                    params![owner, e],
                )
                .unwrap();
            }
        }
    }

    fn book_with(rows: &[ContactRow]) -> ContactBook {
        let dir = TempDir::new().unwrap();
        build_db(&dir.path().join("AddressBook-v22.abcddb"), rows);
        ContactBook::load(dir.path()).unwrap()
    }

    #[test]
    fn formatted_contact_number_matches_e164_handle() {
        let book = book_with(&[("Alice", "Smith", "", &["(916) 555-0100"], &[])]);
        assert_eq!(book.resolve("+19165550100"), Some("Alice Smith"));
    }

    #[test]
    fn e164_contact_number_matches_e164_handle() {
        let book = book_with(&[("Alice", "Smith", "", &["+1 916-555-0100"], &[])]);
        assert_eq!(book.resolve("+19165550100"), Some("Alice Smith"));
    }

    #[test]
    fn email_handles_match_case_insensitively() {
        let book = book_with(&[("Bob", "", "", &[], &["Bob@Example.COM"])]);
        assert_eq!(book.resolve("bob@example.com"), Some("Bob"));
    }

    #[test]
    fn organization_is_the_fallback_name() {
        let book = book_with(&[("", "", "Dentist Office", &["555-0111"], &[])]);
        assert_eq!(book.resolve("5550111"), Some("Dentist Office"));
    }

    #[test]
    fn unknown_handles_resolve_to_none() {
        let book = book_with(&[("Alice", "Smith", "", &["(916) 555-0100"], &[])]);
        assert_eq!(book.resolve("+19990000000"), None);
        assert_eq!(book.resolve("nobody@example.com"), None);
    }

    #[test]
    fn sources_databases_are_merged() {
        let dir = TempDir::new().unwrap();
        build_db(
            &dir.path().join("AddressBook-v22.abcddb"),
            &[("Alice", "Smith", "", &["(916) 555-0100"], &[])],
        );
        let src = dir.path().join("Sources/ABC-123");
        std::fs::create_dir_all(&src).unwrap();
        build_db(
            &src.join("AddressBook-v22.abcddb"),
            &[("Carol", "Jones", "", &["+1 (415) 555-0199"], &[])],
        );
        let book = ContactBook::load(dir.path()).unwrap();
        assert_eq!(book.databases(), 2);
        assert_eq!(book.resolve("+19165550100"), Some("Alice Smith"));
        assert_eq!(book.resolve("+14155550199"), Some("Carol Jones"));
    }

    #[test]
    fn missing_directory_is_an_error() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            ContactBook::load(&dir.path().join("nope")),
            Err(ContactsError::NotFound(_))
        ));
    }

    #[test]
    fn nameless_records_are_skipped() {
        let book = book_with(&[("", "", "", &["555-0122"], &[])]);
        assert_eq!(book.resolve("5550122"), None);
        assert_eq!(book.names(), 0);
    }
}
