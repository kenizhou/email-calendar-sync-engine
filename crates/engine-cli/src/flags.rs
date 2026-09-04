//! The command-line flag parser: a minimal `--flag value` plus positionals
//! grammar shared by every command `cli.rs` dispatches. Kept in the library
//! (like the dispatch itself) so parsing is testable without a process.

use std::collections::HashMap;

use engine_core::{
    ids::AccountId,
    time::{TimeZoneId, UtcDateTime},
};

use crate::{CliError, Horizon};

/// Flags that stand alone — no value follows them on the command line.
const BOOLEAN_FLAGS: &[&str] = &["insecure", "create"];

/// One command's parsed arguments: `--flag value` pairs plus positionals.
pub(crate) struct Flags {
    /// The `--flag value` pairs, keyed by flag name (boolean flags map to
    /// `"1"`).
    pub(crate) map: HashMap<String, String>,
    /// The positional arguments, in order.
    pub(crate) positionals: Vec<String>,
}

impl Flags {
    /// Parses `args` (the arguments after the command name).
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] when a value flag is the last argument.
    pub(crate) fn parse(args: &[String]) -> Result<Self, CliError> {
        let mut map = HashMap::new();
        let mut positionals = Vec::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if let Some(flag) = arg.strip_prefix("--") {
                if BOOLEAN_FLAGS.contains(&flag) {
                    map.insert(flag.to_owned(), "1".to_owned());
                    continue;
                }
                let value = iter
                    .next()
                    .ok_or_else(|| CliError::Usage(format!("--{flag} needs a value")))?;
                map.insert(flag.to_owned(), value.clone());
            } else {
                positionals.push(arg.clone());
            }
        }
        Ok(Self { map, positionals })
    }

    /// The value of `key`, when it was passed.
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    /// Whether a boolean flag was passed.
    pub(crate) fn has(&self, key: &str) -> bool {
        self.map.get(key).is_some_and(|v| v == "1")
    }

    /// The value of `key`, or a usage error naming it.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] when the flag was not passed.
    pub(crate) fn require(&self, key: &str) -> Result<&str, CliError> {
        self.get(key)
            .ok_or_else(|| CliError::Usage(format!("--{key} is required")))
    }

    /// The `--account` id.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] when absent or not a valid account id.
    pub(crate) fn account(&self) -> Result<AccountId, CliError> {
        AccountId::try_from(self.require("account")?)
            .map_err(|_| CliError::Usage("--account is not a valid account id".to_owned()))
    }

    /// The `--zone`, defaulting to UTC.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] when the name is not an IANA zone.
    pub(crate) fn zone(&self) -> Result<TimeZoneId, CliError> {
        let name = self.get("zone").unwrap_or("Etc/UTC");
        TimeZoneId::iana(name).map_err(|_| CliError::Usage("--zone must not be empty".to_owned()))
    }

    /// The `--limit`, defaulting to 20.
    pub(crate) fn limit(&self) -> usize {
        self.get("limit").and_then(|s| s.parse().ok()).unwrap_or(20)
    }

    /// The expansion horizon, defaulting to a wide window when unspecified.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] when a bound is malformed or the window
    /// is empty.
    pub(crate) fn horizon(&self) -> Result<Horizon, CliError> {
        let start = self.day_instant("horizon-start", "2020-01-01")?;
        let end = self.day_instant("horizon-end", "2030-01-01")?;
        Ok(Horizon::new(start, end)?)
    }

    /// Parses a `YYYY-MM-DD` flag into the UTC midnight instant, or `default`.
    fn day_instant(&self, key: &str, default: &str) -> Result<UtcDateTime, CliError> {
        let date = self.get(key).unwrap_or(default);
        format!("{date}T00:00:00Z")
            .parse()
            .map_err(|_| CliError::Usage(format!("--{key} must be YYYY-MM-DD")))
    }
}
