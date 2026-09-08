//! Command dispatch: the `run` entry point, the per-command handlers, and
//! the usage banner.
//!
//! Kept in the library (not `main.rs`) so the whole CLI surface — flag parsing
//! ([`flags`](crate::flags)), command dispatch, and rendered output — is
//! testable; the binary is a thin shim that prints [`run`]'s output. Commands
//! return their output as a string rather than printing, so tests can assert
//! it.

use crate::{
    CliError, Fixture, flags::Flags, ingest, open, reexpand_calendar, search_calendar, search_mail,
};

/// The usage banner, shown for `--help`-less misuse.
pub const USAGE: &str = "\
usage:
  engine-cli ingest   --db <path> --account <id> [--zone <iana>] [--horizon-start <YYYY-MM-DD>] [--horizon-end <YYYY-MM-DD>] <fixture.json>
  engine-cli reexpand --db <path> --account <id> [--zone <iana>] [--horizon-start <YYYY-MM-DD>] [--horizon-end <YYYY-MM-DD>]
  engine-cli search   --db <path> --account <id> --kind <mail|calendar> [--limit <n>] <query...>
  engine-cli eas-sync --db <path> --account <id> [--kind <mail|calendar|contacts>] [--url <u> --user <u> --password <p> | env EAS_LIVE_URL/USER/PASSWORD]
                      [--folder <fid>[,<fid>...]] [--calendar <cid>[,<cid>...]] [--book <bid>[,<bid>...]] [--rounds <n>] [--insecure]
                      (calendar only: [--horizon-start <YYYY-MM-DD>] [--horizon-end <YYYY-MM-DD>] [--zone <iana>] [--create])";

/// Parses `args` (the arguments after the program name) and runs the command,
/// returning the output to print.
///
/// # Errors
///
/// Returns [`CliError::Usage`] for an unknown/missing command or bad flags, or a
/// pipeline error otherwise.
pub async fn run(args: &[String]) -> Result<String, CliError> {
    let Some((command, rest)) = args.split_first() else {
        return Err(CliError::Usage(USAGE.to_owned()));
    };
    let flags = Flags::parse(rest)?;
    match command.as_str() {
        "ingest" => cmd_ingest(&flags).await,
        "reexpand" => cmd_reexpand(&flags).await,
        "search" => cmd_search(&flags).await,
        "eas-sync" => cmd_eas_sync(&flags).await,
        other => Err(CliError::Usage(format!(
            "unknown command {other:?}\n{USAGE}"
        ))),
    }
}

/// The `eas-sync` command: one EAS account through the engine's own sync
/// (see `eas_sync`'s and `eas_pim`'s module docs). Credentials come from
/// the flags or the `EAS_LIVE_*` gates; `--rounds 2` against one `--db` is
/// the full-then-incremental acceptance run. `--kind` picks the family:
/// `mail` (the default), `calendar`, or `contacts`.
async fn cmd_eas_sync(flags: &Flags) -> Result<String, CliError> {
    let account = flags.account()?;
    let target = crate::eas_sync::EasTarget::resolve(
        flags.get("url"),
        flags.get("user"),
        flags.get("password"),
        flags.has("insecure"),
    )?;
    let rounds = flags
        .get("rounds")
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| CliError::Usage("--rounds must be a positive integer".to_owned()))?;
    if rounds == 0 {
        return Err(CliError::Usage("--rounds must be at least 1".to_owned()));
    }
    // Everything that can be a usage error is refused before the store
    // opens, so a mistyped command never leaves a stray `--db` file.
    let kind = match flags.get("kind").unwrap_or("mail") {
        kind @ ("mail" | "calendar" | "contacts") => kind,
        other => {
            return Err(CliError::Usage(format!(
                "--kind must be mail|calendar|contacts, got {other:?}"
            )));
        }
    };
    match kind {
        "mail" => reject(flags, &["calendar", "book", "create"])?,
        "calendar" => reject(flags, &["folder", "book"])?,
        _ => reject(flags, &["folder", "calendar", "create"])?,
    }
    let store = crate::open(flags.require("db")?)?;
    match kind {
        "mail" => {
            let mut folders = Vec::new();
            if let Some(list) = flags.get("folder") {
                for id in list.split(',') {
                    folders.push(engine_core::ids::MailboxId::try_from(id.trim()).map_err(
                        |_| CliError::Usage("--folder ids must be non-empty".to_owned()),
                    )?);
                }
            }
            crate::eas_sync::eas_sync(
                &store,
                &account,
                &target,
                &folders,
                rounds,
                engine_sync::StreamTuning::responsive(),
            )
            .await
        }
        "calendar" => {
            let mut calendars = Vec::new();
            if let Some(list) = flags.get("calendar") {
                for id in list.split(',') {
                    calendars.push(parse_id(id, "--calendar")?);
                }
            }
            let horizon = flags.horizon()?;
            let zone = flags.zone()?;
            crate::eas_pim::eas_sync_calendar(
                &store,
                &account,
                &target,
                &calendars,
                rounds,
                horizon,
                &zone,
                flags.has("create"),
            )
            .await
        }
        _ => {
            let mut books = Vec::new();
            if let Some(list) = flags.get("book") {
                for id in list.split(',') {
                    books.push(parse_id(id, "--book")?);
                }
            }
            crate::eas_pim::eas_sync_contacts(&store, &account, &target, &books, rounds).await
        }
    }
}

/// One comma-separated id flag entry, parsed into its id type.
fn parse_id<'a, T: TryFrom<&'a str>>(id: &'a str, flag: &str) -> Result<T, CliError> {
    T::try_from(id.trim()).map_err(|_| CliError::Usage(format!("{flag} ids must be non-empty")))
}

/// Refuses flags that belong to another `eas-sync` kind — silently
/// ignoring them would let a mistyped command "succeed" without doing
/// what was asked.
fn reject(flags: &Flags, owned_elsewhere: &[&str]) -> Result<(), CliError> {
    for flag in owned_elsewhere {
        if flags.get(flag).is_some() {
            return Err(CliError::Usage(format!(
                "--{flag} does not apply to this --kind"
            )));
        }
    }
    Ok(())
}

async fn cmd_ingest(flags: &Flags) -> Result<String, CliError> {
    let account = flags.account()?;
    let horizon = flags.horizon()?;
    let zone = flags.zone()?;
    let path = flags
        .positionals
        .first()
        .ok_or_else(|| CliError::Usage("ingest needs a fixture path".to_owned()))?;
    let json = std::fs::read_to_string(path).map_err(|e| CliError::Fixture(e.to_string()))?;
    let fixture = Fixture::from_json(&json)?;
    let store = open(flags.require("db")?)?;
    let report = ingest(&store, account, &fixture, &horizon, &zone).await?;
    Ok(format!(
        "ingested {} messages, {} events, {} occurrences",
        report.messages, report.events, report.occurrences
    ))
}

async fn cmd_reexpand(flags: &Flags) -> Result<String, CliError> {
    let account = flags.account()?;
    let horizon = flags.horizon()?;
    let zone = flags.zone()?;
    let store = open(flags.require("db")?)?;
    let occurrences = reexpand_calendar(&store, account, &horizon, &zone).await?;
    Ok(format!("re-expanded {occurrences} occurrences"))
}

async fn cmd_search(flags: &Flags) -> Result<String, CliError> {
    let account = flags.account()?;
    let limit = flags.limit();
    let query = flags.positionals.join(" ");
    let store = open(flags.require("db")?)?;
    let results = match flags.require("kind")? {
        "mail" => search_mail(&store, account, &query, limit).await?,
        "calendar" => search_calendar(&store, account, &query, limit).await?,
        other => {
            return Err(CliError::Usage(format!(
                "--kind must be mail|calendar, got {other:?}"
            )));
        }
    };
    let mut lines: Vec<String> = results
        .hits
        .iter()
        .map(|hit| format!("{}\t{:.4}", hit.key.as_str(), hit.score))
        .collect();
    lines.push(format!(
        "coverage: complete={}",
        results.coverage.is_complete()
    ));
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use engine_core::{
        calendar::{Event, Frequency, Recurrence, RecurrenceRule},
        ids::{CalendarId, EventId, Uid},
        membership::Memberships,
        time::{CalendarDateTime, LocalDateTime},
    };

    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn write_fixture(dir: &std::path::Path) -> String {
        let mut event = Event::new(
            EventId::try_from("daily").unwrap(),
            Uid::new("u-daily").unwrap(),
            Memberships::of_one(CalendarId::try_from("cal").unwrap()),
            CalendarDateTime::utc(LocalDateTime::new(2026, 6, 1, 9, 0, 0).unwrap()),
        );
        event.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(Frequency::Daily)));
        let json = serde_json::to_string(&serde_json::json!({ "events": [event] })).unwrap();
        let path = dir.join("fixture.json");
        std::fs::write(&path, json).unwrap();
        path.to_str().unwrap().to_owned()
    }

    /// `--rounds 0` is refused before the store is even opened.
    #[tokio::test]
    async fn eas_sync_refuses_zero_rounds() {
        let err = run(&args(&[
            "eas-sync",
            "--db",
            "unused",
            "--account",
            "acct-1",
            "--url",
            "http://server.invalid/Microsoft-Server-ActiveSync",
            "--user",
            "user@example.test",
            "--password",
            "app-password",
            "--rounds",
            "0",
        ]))
        .await
        .expect_err("zero rounds is a usage error");
        assert!(
            err.to_string().contains("--rounds"),
            "the error names the flag: {err}"
        );
    }

    /// An unknown `--kind` is refused before the store is even opened.
    #[tokio::test]
    async fn eas_sync_refuses_unknown_kind() {
        let err = run(&args(&[
            "eas-sync",
            "--db",
            "unused",
            "--account",
            "acct-1",
            "--url",
            "http://server.invalid/Microsoft-Server-ActiveSync",
            "--user",
            "user@example.test",
            "--password",
            "app-password",
            "--rounds",
            "1",
            "--kind",
            "tasks",
        ]))
        .await
        .expect_err("an unknown kind is a usage error");
        assert!(
            err.to_string().contains("--kind"),
            "the error names the flag: {err}"
        );
    }

    /// Flags that belong to another `--kind` are refused, not silently
    /// ignored — a mistyped command must not "succeed" without doing what
    /// was asked.
    #[tokio::test]
    async fn eas_sync_refuses_flags_of_another_kind() {
        for (kind, flag) in [
            ("contacts", "--folder"),
            ("contacts", "--create"),
            ("calendar", "--book"),
            ("mail", "--create"),
        ] {
            let mut call = args(&[
                "eas-sync",
                "--db",
                "unused",
                "--account",
                "acct-1",
                "--url",
                "http://server.invalid/Microsoft-Server-ActiveSync",
                "--user",
                "user@example.test",
                "--password",
                "app-password",
                "--rounds",
                "1",
                "--kind",
                kind,
            ]);
            if flag == "--create" {
                call.push("--create".to_owned());
            } else {
                call.push(flag.to_owned());
                call.push("some-id".to_owned());
            }
            let err = run(&call)
                .await
                .expect_err("a foreign flag is a usage error");
            assert!(
                err.to_string().contains(flag),
                "the error names the flag {flag}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn ingest_then_search_via_run() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let db = db.to_str().unwrap();
        let fixture = write_fixture(dir.path());

        let out = run(&args(&[
            "ingest",
            "--db",
            db,
            "--account",
            "acct-1",
            "--horizon-start",
            "2026-06-01",
            "--horizon-end",
            "2026-06-04",
            &fixture,
        ]))
        .await
        .unwrap();
        assert!(out.contains("3 occurrences"), "{out}");

        let found = run(&args(&[
            "search",
            "--db",
            db,
            "--account",
            "acct-1",
            "--kind",
            "calendar",
            "after:2026-06-01 before:2026-06-04",
        ]))
        .await
        .unwrap();
        assert!(found.contains("daily"));
        assert!(found.contains("complete=true"));

        let occ = run(&args(&[
            "reexpand",
            "--db",
            db,
            "--account",
            "acct-1",
            "--horizon-start",
            "2026-06-01",
            "--horizon-end",
            "2026-06-10",
        ]))
        .await
        .unwrap();
        assert!(occ.contains("re-expanded 9 occurrences"), "{occ}");
    }

    #[tokio::test]
    async fn usage_and_flag_errors() {
        assert!(matches!(run(&[]).await, Err(CliError::Usage(_))));
        assert!(matches!(
            run(&args(&["frobnicate"])).await,
            Err(CliError::Usage(_))
        ));
        // A flag missing its value.
        assert!(matches!(
            run(&args(&["ingest", "--db"])).await,
            Err(CliError::Usage(_))
        ));
        // Missing required --account.
        assert!(matches!(
            run(&args(&["ingest", "--db", "x.sqlite"])).await,
            Err(CliError::Usage(_))
        ));
        // Bad search kind.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let bad_kind = run(&args(&[
            "search",
            "--db",
            db.to_str().unwrap(),
            "--account",
            "acct-1",
            "--kind",
            "contacts",
            "q",
        ]))
        .await;
        assert!(matches!(bad_kind, Err(CliError::Usage(_))));
    }

    #[tokio::test]
    async fn bad_horizon_and_missing_fixture_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let db = db.to_str().unwrap();
        // horizon-start after horizon-end is an empty window.
        let empty = run(&args(&[
            "reexpand",
            "--db",
            db,
            "--account",
            "acct-1",
            "--horizon-start",
            "2026-06-10",
            "--horizon-end",
            "2026-06-01",
        ]))
        .await;
        assert!(empty.is_err());
        // A malformed horizon date.
        let bad_date = run(&args(&[
            "reexpand",
            "--db",
            db,
            "--account",
            "acct-1",
            "--horizon-start",
            "nonsense",
        ]))
        .await;
        assert!(matches!(bad_date, Err(CliError::Usage(_))));
        // A fixture path that does not exist.
        let missing = run(&args(&[
            "ingest",
            "--db",
            db,
            "--account",
            "acct-1",
            "/no/such/fixture.json",
        ]))
        .await;
        assert!(matches!(missing, Err(CliError::Fixture(_))));
    }
}
