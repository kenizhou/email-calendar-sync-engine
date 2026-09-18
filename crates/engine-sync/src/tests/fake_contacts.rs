//! The fake's contacts surface, split from the shared scaffolding in `mod.rs`
//! at the 500-line cap. Fork-owned (`FORKING.md`); the trait keeps its erroring
//! defaults for every verb a happy-path drive never reaches.

use engine_provider::ContactsProvider;

use super::{AccountId, ContactDraft, ContactWriteReceipt, FakeMail, ProviderResult};

/// A create returns a canned receipt echoing the draft's card id (the one verb
/// a happy-path contact drive needs). Every other contact verb keeps the
/// trait's erroring defaults, which no test here should reach — the gone-card
/// paths resolve without a provider call.
#[async_trait::async_trait]
impl ContactsProvider for FakeMail {
    async fn create_contact(
        &self,
        _account: &AccountId,
        draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        Ok(ContactWriteReceipt::new(draft.card.id.clone()))
    }
}
