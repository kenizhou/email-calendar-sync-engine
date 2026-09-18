//! The fork's person-schema migration step. Fork-owned
//! (`FORKING.md` patch series, `person.display_name` nullable row); split out of
//! `schema.rs` when the merged file crossed the 500-line cap, following the
//! `mail.rs` precedent for steps that live beside the main list.

/// Migration v15 (fork): `person.display_name` goes nullable — renumbered from
/// the fork's v14 when upstream's own v14 (the queue columns) landed.
///
/// The model has been `Option<String>` since the people index landed
/// (`Person::display_name`): a person whose sources carry neither a name nor an
/// address is deliberately `None` — naming the nameless is a host-side presentation
/// decision, not something a provider-neutral core invents. The v7 table contradicted
/// the model with `NOT NULL`, so every contacts collection holding such a card
/// faulted its whole people replacement (`NOT NULL constraint failed:
/// person.display_name`). No SQL reads the column — every reader goes through
/// `payload`, and the `person_display_name` index has no user — so relaxing it breaks
/// nothing. SQLite cannot drop a constraint with `ALTER`, so the table is rebuilt
/// copy-forward: nothing references `person` across tables, and the copied column is
/// itself `NOT NULL`, so the copy cannot fail.
pub(crate) const V15: &str = "\
CREATE TABLE person_v15 (
    id           INTEGER NOT NULL PRIMARY KEY,
    ordinal      INTEGER NOT NULL UNIQUE,
    display_name TEXT,
    payload      TEXT    NOT NULL
) STRICT;

INSERT INTO person_v15 (id, ordinal, display_name, payload)
SELECT id, ordinal, display_name, payload FROM person;

DROP TABLE person;

ALTER TABLE person_v15 RENAME TO person;

CREATE INDEX person_display_name ON person (display_name, id);
";
