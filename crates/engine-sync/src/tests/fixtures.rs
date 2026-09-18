//! The small fixture helpers (accounts, clocks, workers, mailboxes, messages,
//! drafts, provider keys) the themed submodules reach via `use super::*`.
//! Split from `mod.rs` at the 500-line cap; byte-identical moves.

use super::*;

pub(crate) fn draft(message_id: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body",
    )
}

pub(crate) fn mailbox(id: &str, name: &str, role: Option<MailboxRole>) -> Mailbox {
    let mut mailbox = Mailbox::new(MailboxId::try_from(id).unwrap(), name);
    mailbox.role = role;
    mailbox
}

pub(crate) fn message(id: &str, mailbox: &str, subject: &str) -> Message {
    let mut message = Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
    );
    message.envelope.subject = Some(subject.to_owned());
    message
}

pub(crate) fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

pub(crate) fn clock() -> ManualClock {
    ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap())
}

pub(crate) fn worker() -> WorkerId {
    WorkerId::new("w-1")
}

pub(crate) fn key(value: &str) -> ProviderKey {
    ProviderKey::new(value).unwrap()
}
