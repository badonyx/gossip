use crate::comms::ToOverlordMessage;
use crate::error::{Error, ErrorKind};
use crate::globals::GLOBALS;
use dashmap::{DashMap, DashSet};
use gossip_relay_picker::Direction;
use image::RgbaImage;
use nostr_types::{
    ContentEncryptionAlgorithm, Event, EventKind, Metadata, PreEvent, PublicKey, RelayUrl, Tag,
    UncheckedUrl, Unixtime, Url,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::task;

/// Person type, aliased to the latest version
pub type Person = crate::storage::types::Person2;

/// PersonList type, aliased to the latest version
pub type PersonList = crate::storage::types::PersonList1;

/// PersonListMetadata type, aliased to the latest version
pub type PersonListMetadata = crate::storage::types::PersonListMetadata1;

/// Person List Compare Data
#[derive(Debug, Clone)]
pub struct PersonListEventData {
    /// The timestamp of the latest event
    pub when: Unixtime,

    /// The number of public entries in the latest event
    pub public_len: usize,

    /// The number of private entires in the latest event, or None if it
    /// couldn't be computed (not logged in, Following event, or none found)
    pub private_len: Option<usize>,
}

impl Default for PersonListEventData {
    fn default() -> PersonListEventData {
        PersonListEventData {
            when: Unixtime(0),
            public_len: 0,
            private_len: None,
        }
    }
}

/// Handles people and remembers what needs to be done for each, such as fetching
/// metadata or avatars.
pub struct People {
    // active person's relays (pull from db as needed)
    active_person: RwLock<Option<PublicKey>>,
    active_persons_write_relays: RwLock<Vec<(RelayUrl, u64)>>,

    // We fetch (with Fetcher), process, and temporarily hold avatars
    // until the UI next asks for them, at which point we remove them
    // and hand them over. This way we can do the work that takes
    // longer and the UI can do as little work as possible.
    avatars_temp: DashMap<PublicKey, RgbaImage>,
    avatars_pending_processing: DashSet<PublicKey>,

    // When we manually ask for updating metadata, we want to recheck
    // the person's NIP-05 when that metadata come in. We remember this here.
    recheck_nip05: DashSet<PublicKey>,

    // People of interest that the UI is showing, whose metadata should
    // be updated if it is stale.
    people_of_interest: DashSet<PublicKey>,

    // Metadata fetches in progress. Once these get too old we can remove them
    // and consider them to have failed.
    // This only relates to the Metadata event, not subsequent avatar or nip05
    // loads.
    fetching_metadata: DashMap<PublicKey, Unixtime>,

    /// Latest person list event data for each PersonList
    pub latest_person_list_event_data: DashMap<PersonList, PersonListEventData>,
}

impl Default for People {
    fn default() -> Self {
        Self::new()
    }
}

impl People {
    pub(crate) fn new() -> People {
        People {
            active_person: RwLock::new(None),
            active_persons_write_relays: RwLock::new(vec![]),
            avatars_temp: DashMap::new(),
            avatars_pending_processing: DashSet::new(),
            recheck_nip05: DashSet::new(),
            people_of_interest: DashSet::new(),
            fetching_metadata: DashMap::new(),
            latest_person_list_event_data: DashMap::new(),
        }
    }

    // Start the periodic task management
    pub(crate) fn start() {
        GLOBALS.people.update_latest_person_list_event_data();

        task::spawn(async {
            loop {
                let fetch_metadata_looptime_ms =
                    GLOBALS.storage.read_setting_fetcher_metadata_looptime_ms();

                // Every 3 seconds...
                tokio::time::sleep(Duration::from_millis(fetch_metadata_looptime_ms)).await;

                // We fetch needed metadata
                GLOBALS.people.maybe_fetch_metadata().await;

                // And we check for shutdown condition
                if GLOBALS.shutting_down.load(Ordering::Relaxed) {
                    break;
                }
            }
        });
    }

    /// Search local events for the latest PersonList event for each kind of PersonList,
    /// and determine their timestamps and lengths, storing result in People.
    pub fn update_latest_person_list_event_data(&self) {
        // Get public key, or give up
        let pk = match GLOBALS.storage.read_setting_public_key() {
            Some(pk) => pk,
            None => return,
        };

        for (person_list, _) in PersonList::all_lists() {
            if let Ok(Some(event)) = GLOBALS.storage.get_replaceable_event(
                person_list.event_kind(),
                pk,
                &person_list.name(),
            ) {
                self.latest_person_list_event_data.insert(
                    person_list,
                    PersonListEventData {
                        when: event.created_at,
                        public_len: event
                            .tags
                            .iter()
                            .filter(|t| matches!(t, Tag::Pubkey { .. }))
                            .count(),
                        private_len: {
                            let mut private_len: Option<usize> = None;
                            if !matches!(person_list, PersonList::Followed)
                                && GLOBALS.signer.is_ready()
                            {
                                if let Ok(bytes) = GLOBALS.signer.decrypt_nip04(&pk, &event.content)
                                {
                                    if let Ok(vectags) = serde_json::from_slice::<Vec<Tag>>(&bytes)
                                    {
                                        private_len = Some(vectags.len());
                                    }
                                }
                            }
                            private_len
                        },
                    },
                );
            } else {
                self.latest_person_list_event_data.remove(&person_list);
            }
        }
    }

    /// Get all the pubkeys that the user subscribes to in any list
    pub fn get_subscribed_pubkeys(&self) -> Vec<PublicKey> {
        // We subscribe to all people in all lists.
        // This is no longer synonomous with the ContactList list
        match GLOBALS.storage.get_people_in_all_followed_lists() {
            Ok(people) => people,
            Err(e) => {
                tracing::error!("{}", e);
                vec![]
            }
        }
    }

    /// Is the person in the list? (returns false on error)
    #[inline]
    pub fn is_person_in_list(&self, pubkey: &PublicKey, list: PersonList) -> bool {
        GLOBALS
            .storage
            .is_person_in_list(pubkey, list)
            .unwrap_or(false)
    }

    /// Get all the pubkeys that need relay lists (from the given set)
    pub fn get_subscribed_pubkeys_needing_relay_lists(
        &self,
        among_these: &[PublicKey],
    ) -> Vec<PublicKey> {
        let stale = Unixtime::now().unwrap().0
            - 60 * 60
                * GLOBALS
                    .storage
                    .read_setting_relay_list_becomes_stale_hours() as i64;

        if let Ok(vec) = GLOBALS.storage.filter_people(|p| {
            p.is_subscribed_to()
                && p.relay_list_last_received < stale
                && among_these.contains(&p.pubkey)
        }) {
            vec.iter().map(|p| p.pubkey).collect()
        } else {
            vec![]
        }
    }

    /// Create person record for this pubkey, if missing
    pub fn create_if_missing(&self, pubkey: PublicKey) {
        if let Err(e) = self.create_all_if_missing(&[pubkey]) {
            tracing::error!("{}", e);
        }
    }

    /// Create person records for these pubkeys, if missing
    pub fn create_all_if_missing(&self, pubkeys: &[PublicKey]) -> Result<(), Error> {
        for pubkey in pubkeys {
            GLOBALS.storage.write_person_if_missing(pubkey, None)?;
        }

        Ok(())
    }

    /// Mark this person as a person who the UI wants fresh metadata for.
    /// maybe_fetch_metadata() will do the processing later on.
    pub fn person_of_interest(&self, pubkey: PublicKey) {
        // Don't set if metadata if disabled
        if !GLOBALS.storage.read_setting_automatically_fetch_metadata() {
            return;
        }

        self.people_of_interest.insert(pubkey);
    }

    /// The overlord calls this to indicate that it is fetching metadata
    /// for this person from relays
    pub fn metadata_fetch_initiated(&self, pubkeys: &[PublicKey]) {
        let now = Unixtime::now().unwrap();
        for pubkey in pubkeys {
            self.fetching_metadata.insert(*pubkey, now);
        }
    }

    /// This is run periodically. It checks the database first, only then does it
    /// ask the overlord to update the metadata from the relays.
    async fn maybe_fetch_metadata(&self) {
        // Take everybody out of self.people_of_interest, into a local var
        let mut people_of_interest: Vec<PublicKey> = self
            .people_of_interest
            .iter()
            .map(|refmulti| refmulti.key().to_owned())
            .collect();
        self.people_of_interest.clear();

        if !people_of_interest.is_empty() {
            tracing::trace!(
                "Periodic metadata check against {} people",
                people_of_interest.len()
            );
        }

        let now = Unixtime::now().unwrap();
        let stale = Duration::from_secs(
            60 * 60 * GLOBALS.storage.read_setting_metadata_becomes_stale_hours(),
        );

        let mut verified_need: Vec<PublicKey> = Vec::new();

        for pubkey in people_of_interest.drain(..) {
            // If we already tried fetching_metadata (within the stale period)
            // skip them
            // NOTE: if we tried and it never came in, odds are low that trying
            // again will make any difference. Either the person doesn't have
            // metadata or we don't have their proper relays. So a shorter timeout
            // in this circumstance isn't such a great idea.
            if let Some(fetching_asof) = self.fetching_metadata.get(&pubkey) {
                if fetching_asof.0 >= (now - stale).0 {
                    continue;
                } else {
                    // remove stale entry
                    self.fetching_metadata.remove(&pubkey);
                }
            }

            match GLOBALS.storage.read_person(&pubkey) {
                Ok(Some(person)) => {
                    // We need metadata if it is missing or old
                    let need = {
                        // Metadata refresh interval
                        person.metadata_created_at.is_none()
                            || person.metadata_last_received < (now - stale).0
                    };
                    if !need {
                        continue;
                    }

                    tracing::debug!("Seeking metadata for {}", pubkey.as_hex_string());
                    verified_need.push(pubkey);
                }
                _ => {
                    // Trigger a future create and load
                    self.create_if_missing(pubkey);
                    // Don't load metadata now, we may have it on disk and get
                    // it from the future load.
                }
            }
        }

        // This fires off the minions to fetch metadata events
        // When they come in, process.rs handles it by calling
        // GLOBALS.people.update_metadata() [down below]
        let _ = GLOBALS
            .to_overlord
            .send(ToOverlordMessage::UpdateMetadataInBulk(verified_need));
    }

    pub(crate) fn recheck_nip05_on_update_metadata(&self, pubkey: &PublicKey) {
        self.recheck_nip05.insert(pubkey.to_owned());
    }

    pub(crate) async fn update_metadata(
        &self,
        pubkey: &PublicKey,
        metadata: Metadata,
        asof: Unixtime,
    ) -> Result<(), Error> {
        // Remove from fetching metadata (fetch is complete)
        self.fetching_metadata.remove(pubkey);

        // Sync in from database first
        self.create_all_if_missing(&[*pubkey])?;

        let now = Unixtime::now().unwrap();

        // Copy the person
        let mut person = GLOBALS
            .storage
            .read_person(pubkey)?
            .unwrap_or(Person::new(pubkey.to_owned()));

        // Update metadata_last_received, even if we don't update the metadata
        person.metadata_last_received = now.0;
        GLOBALS.storage.write_person(&person, None)?;

        // Determine whether it is fresh
        let fresh = match person.metadata_created_at {
            Some(metadata_created_at) => asof.0 > metadata_created_at,
            None => true,
        };

        if fresh {
            let nip05_changed = if let Some(md) = &person.metadata {
                metadata.nip05 != md.nip05.clone()
            } else {
                metadata.nip05.is_some()
            };

            // Update person in the map, and the local variable
            person.metadata = Some(metadata);
            person.metadata_created_at = Some(asof.0);
            if nip05_changed {
                person.nip05_valid = false; // changed, so reset to invalid
                person.nip05_last_checked = None; // we haven't checked this one yet
            }
            GLOBALS.storage.write_person(&person, None)?;
            GLOBALS.ui_people_to_invalidate.write().push(*pubkey);
        }

        // Remove from failed avatars list so the UI will try to fetch the avatar again if missing
        GLOBALS.failed_avatars.write().await.remove(pubkey);

        // Only if they have a nip05 dns id set
        if matches!(
            person,
            Person {
                metadata: Some(Metadata { nip05: Some(_), .. }),
                ..
            }
        ) {
            // Recheck nip05 every day if invalid, and every two weeks if valid

            let recheck = {
                if self.recheck_nip05.contains(pubkey) {
                    self.recheck_nip05.remove(pubkey);
                    true
                } else if let Some(last) = person.nip05_last_checked {
                    // FIXME make these settings
                    let recheck_duration = if person.nip05_valid {
                        Duration::from_secs(
                            60 * 60
                                * GLOBALS
                                    .storage
                                    .read_setting_nip05_becomes_stale_if_valid_hours(),
                        )
                    } else {
                        Duration::from_secs(
                            60 * GLOBALS
                                .storage
                                .read_setting_nip05_becomes_stale_if_invalid_minutes(),
                        )
                    };
                    Unixtime::now().unwrap() - Unixtime(last as i64) > recheck_duration
                } else {
                    true
                }
            };

            if recheck {
                self.update_nip05_last_checked(person.pubkey).await?;
                task::spawn(async move {
                    if let Err(e) = crate::nip05::validate_nip05(person).await {
                        tracing::error!("{}", e);
                    }
                });
            }
        }

        Ok(())
    }

    /// Get the avatar `RgbaImage` for the person.
    ///
    /// This usually returns None when first called, and eventually returns the image.
    /// Once the image is returned, it will return None ever after, because the image is
    /// moved, not copied.
    ///
    /// FIXME this API is not good for async front ends.
    pub fn get_avatar(
        &self,
        pubkey: &PublicKey,
        rounded: bool,
        avatar_size: u32,
    ) -> Option<RgbaImage> {
        // If we have it, hand it over (we won't need a copy anymore)
        if let Some(th) = self.avatars_temp.remove(pubkey) {
            return Some(th.1);
        }

        // If it failed before, error out now
        if GLOBALS.failed_avatars.blocking_read().contains(pubkey) {
            return None; // cannot recover.
        }

        // If it is pending processing, respond now
        if self.avatars_pending_processing.contains(pubkey) {
            return None; // will recover after processing completes
        }

        // Do not fetch if disabled
        if !GLOBALS.storage.read_setting_load_avatars() {
            return None; // can recover if the setting is switched
        }

        // Get the person this is about
        let person = match GLOBALS.storage.read_person(pubkey) {
            Ok(Some(person)) => person,
            _ => return None, // can recover once the person is loaded
        };

        // Fail permanently if they don't have a picture url
        if person.picture().is_none() {
            // this cannot recover without new metadata
            GLOBALS.failed_avatars.blocking_write().insert(*pubkey);

            return None;
        }

        // Fail permanently if the URL is bad
        let url = UncheckedUrl(person.picture().unwrap().to_string());
        let url = match Url::try_from_unchecked_url(&url) {
            Ok(url) => url,
            Err(_) => {
                // this cannot recover without new metadata
                GLOBALS.failed_avatars.blocking_write().insert(*pubkey);

                return None;
            }
        };

        match GLOBALS.fetcher.try_get(
            &url,
            Duration::from_secs(
                60 * 60 * GLOBALS.storage.read_setting_avatar_becomes_stale_hours(),
            ),
        ) {
            // cache expires in 3 days
            Ok(None) => None,
            Ok(Some(bytes)) => {
                // Finish this later (spawn)
                let apubkey = *pubkey;
                tokio::spawn(async move {
                    let size = avatar_size * 3 // 3x feed size, 1x people page size
                        * GLOBALS
                            .pixels_per_point_times_100
                            .load(Ordering::Relaxed)
                        / 100;

                    match crate::media::load_image_bytes(
                        &bytes, true, // crop square
                        size, // default size,
                        true, // force to that size
                        rounded,
                    ) {
                        Ok(color_image) => {
                            GLOBALS.people.avatars_temp.insert(apubkey, color_image);
                        }
                        Err(_) => {
                            // this cannot recover without new metadata
                            GLOBALS.failed_avatars.write().await.insert(apubkey);
                        }
                    }
                });
                self.avatars_pending_processing.insert(pubkey.to_owned());
                None
            }
            Err(e) => {
                tracing::error!("{}", e);
                // this cannot recover without new metadata
                GLOBALS.failed_avatars.blocking_write().insert(*pubkey);
                None
            }
        }
    }

    /// This lets you start typing a name, and autocomplete the results for tagging
    /// someone in a post.  It returns maximum 10 results.
    pub fn search_people_to_tag(&self, mut text: &str) -> Result<Vec<(String, PublicKey)>, Error> {
        // work with or without the @ symbol:
        if text.starts_with('@') {
            text = &text[1..]
        }
        // normalize case
        let search = String::from(text).to_lowercase();

        // grab all results then sort by score
        let mut results: Vec<(u16, String, PublicKey)> = GLOBALS
            .storage
            .filter_people(|_| true)?
            .iter()
            .filter_map(|person| {
                let mut score = 0u16;
                let mut result_name = String::from("");

                // search for users by name
                let name = person.best_name();
                let matchable = name.to_lowercase();
                if matchable.starts_with(&search) {
                    score = 300;
                    result_name = name.to_string();
                }
                if matchable.contains(&search) {
                    score = 200;
                    result_name = name.to_string();
                }

                // search for users by nip05 id
                if score == 0 && person.nip05_valid {
                    if let Some(nip05) = &person.nip05().map(|n| n.to_lowercase()) {
                        if nip05.starts_with(&search) {
                            score = 400;
                            result_name = nip05.to_string();
                        }
                        if nip05.contains(&search) {
                            score = 100;
                            result_name = nip05.to_string();
                        }
                    }
                }

                if score > 0 {
                    // if there is not a name, fallback to showing the initial chars of the pubkey,
                    // but this is probably unnecessary and will never happen
                    if result_name.is_empty() {
                        result_name = person.pubkey.as_hex_string();
                    }

                    // bigger names have a higher match chance, but they should be scored lower
                    score -= result_name.len() as u16;

                    return Some((score, result_name, person.pubkey));
                }

                None
            })
            .collect();

        results.sort_by(|a, b| a.0.cmp(&b.0).reverse());
        let max = if results.len() > 10 {
            10
        } else {
            results.len()
        };

        Ok(results[0..max]
            .iter()
            .map(|r| (r.1.to_owned(), r.2.to_owned()))
            .collect())
    }

    fn preserve_tags(old_tags: &[Tag], new_kind: EventKind) -> Vec<Tag> {
        let mut tags = Vec::new();

        if new_kind == EventKind::MuteList {
            // For mute lists, preserve 't', 'e' and 'word' tags from the previous event
            // so as to not clobber them, they may be used on other clients
            for t in old_tags {
                match t {
                    Tag::Hashtag { .. } => {
                        tags.push(t.clone());
                    }
                    Tag::Event { .. } => {
                        tags.push(t.clone());
                    }
                    Tag::Other { tag, .. } => {
                        if tag == "word" {
                            tags.push(t.clone());
                        }
                    }
                    _ => (),
                }
            }
        } else if new_kind == EventKind::FollowSets {
            // For FollowSets we should preserve "title", "image" and "description"
            for t in old_tags {
                match t {
                    Tag::Title { .. } => {
                        tags.push(t.clone());
                    }
                    Tag::Other { tag, .. } => {
                        if tag == "image" || tag == "description" {
                            tags.push(t.clone());
                        }
                    }
                    _ => (),
                }
            }
        }
        // For ContactList we don't need to preserve anything (see NIP-02)

        tags
    }

    pub(crate) async fn generate_person_list_event(
        &self,
        person_list: PersonList,
    ) -> Result<Event, Error> {
        if !GLOBALS.signer.is_ready() {
            return Err((ErrorKind::NoPrivateKey, file!(), line!()).into());
        }

        let my_pubkey = GLOBALS.signer.public_key().unwrap();

        // Read the person list in two parts
        let people = GLOBALS.storage.get_people_in_list(person_list)?;

        // Determine the event kind
        let kind = match person_list {
            PersonList::Followed => EventKind::ContactList,
            PersonList::Muted => EventKind::MuteList,
            PersonList::Custom(_) => EventKind::FollowSets,
        };

        // Pull the existing event
        let existing_event: Option<Event> = match kind {
            EventKind::ContactList | EventKind::MuteList => {
                // We fetch for ContactList to preserve the contents
                // We fetch for MuteList to preserve 't', 'e', and "word" tags
                GLOBALS.storage.get_replaceable_event(kind, my_pubkey, "")?
            }
            EventKind::FollowSets => {
                // We fetch for FollowSets to preserve various tags we don't use
                GLOBALS
                    .storage
                    .get_replaceable_event(kind, my_pubkey, &person_list.name())?
            }
            _ => None,
        };

        // Build the public tags
        let public_tags: Vec<Tag> = {
            let mut tags = Vec::new();

            // Preserve public tags from existing event
            if let Some(ref event) = existing_event {
                tags = Self::preserve_tags(&event.tags, kind);
            }

            for (pubkey, public) in people.iter() {
                if !*public {
                    continue;
                }

                // Only include petnames in the ContactList (which is only public people)
                let petname = if kind == EventKind::ContactList {
                    if let Some(person) = GLOBALS.storage.read_person(pubkey)? {
                        person.petname.clone()
                    } else {
                        None
                    }
                } else {
                    None
                };

                // Only include recommended relay urls in public entries, and not in the mute list
                let recommended_relay_url = if kind != EventKind::MuteList {
                    let relays = GLOBALS.storage.get_best_relays(*pubkey, Direction::Write)?;
                    relays.get(0).map(|(u, _)| u.to_unchecked_url())
                } else {
                    None
                };

                tags.push(Tag::Pubkey {
                    pubkey: pubkey.into(),
                    recommended_relay_url,
                    petname,
                    trailing: vec![],
                });
            }

            // Add d-tag if using FollowSets
            if matches!(person_list, PersonList::Custom(_)) {
                tags.push(Tag::Identifier {
                    d: person_list.name(),
                    trailing: vec![],
                });
            }

            tags
        };

        let content = {
            if kind == EventKind::ContactList {
                // Preserve the contents of any existing kind-3 event for use by
                // other clients
                match existing_event {
                    Some(c) => c.content,
                    None => "".to_owned(),
                }
            } else {
                let private_tags: Vec<Tag> = {
                    let mut tags = Vec::new();

                    // Preserve private tags from existing event
                    if let Some(ref event) = existing_event {
                        if person_list != PersonList::Followed && !event.content.is_empty() {
                            let decrypted_content =
                                GLOBALS.signer.decrypt_nip04(&my_pubkey, &event.content)?;
                            let old_tags: Vec<Tag> = serde_json::from_slice(&decrypted_content)?;
                            tags = Self::preserve_tags(&old_tags, kind);
                        }
                    }

                    // Build private tags (except for ContactList)
                    for (pubkey, public) in people.iter() {
                        if *public {
                            continue;
                        }

                        tags.push(Tag::Pubkey {
                            pubkey: pubkey.into(),
                            recommended_relay_url: None,
                            petname: None,
                            trailing: vec![],
                        });
                    }

                    tags
                };

                let private_tags_string = serde_json::to_string(&private_tags)?;
                GLOBALS.signer.encrypt(
                    &my_pubkey,
                    &private_tags_string,
                    ContentEncryptionAlgorithm::Nip04,
                )?
            }
        };

        let pre_event = PreEvent {
            pubkey: my_pubkey,
            created_at: Unixtime::now().unwrap(),
            kind,
            tags: public_tags,
            content,
        };

        GLOBALS.signer.sign_preevent(pre_event, None, None)
    }

    /// Follow (or unfollow) the public key
    pub fn follow(
        &self,
        pubkey: &PublicKey,
        follow: bool,
        list: PersonList,
        public: bool,
    ) -> Result<(), Error> {
        let mut txn = GLOBALS.storage.get_write_txn()?;

        if follow {
            GLOBALS
                .storage
                .add_person_to_list(pubkey, list, public, Some(&mut txn))?;
        } else {
            GLOBALS
                .storage
                .remove_person_from_list(pubkey, list, Some(&mut txn))?;
        }
        GLOBALS.ui_people_to_invalidate.write().push(*pubkey);

        GLOBALS.storage.set_person_list_last_edit_time(
            list,
            Unixtime::now().unwrap().0,
            Some(&mut txn),
        )?;

        txn.commit()?;

        Ok(())
    }

    /// Clear a person list
    pub(crate) fn clear_person_list(&self, list: PersonList) -> Result<(), Error> {
        let mut txn = GLOBALS.storage.get_write_txn()?;

        GLOBALS.storage.clear_person_list(list, Some(&mut txn))?;
        GLOBALS.storage.set_person_list_last_edit_time(
            list,
            Unixtime::now().unwrap().0,
            Some(&mut txn),
        )?;

        txn.commit()?;

        GLOBALS.ui_invalidate_all.store(false, Ordering::Relaxed);

        Ok(())
    }

    /// Mute (or unmute) a public key
    pub fn mute(&self, pubkey: &PublicKey, mute: bool, public: bool) -> Result<(), Error> {
        let mut txn = GLOBALS.storage.get_write_txn()?;

        if mute {
            if let Some(pk) = GLOBALS.signer.public_key() {
                if pk == *pubkey {
                    return Err(ErrorKind::General("You cannot mute yourself".to_owned()).into());
                }
            }

            GLOBALS.storage.add_person_to_list(
                pubkey,
                PersonList::Muted,
                public,
                Some(&mut txn),
            )?;
        } else {
            GLOBALS
                .storage
                .remove_person_from_list(pubkey, PersonList::Muted, Some(&mut txn))?;
        }

        GLOBALS.storage.set_person_list_last_edit_time(
            PersonList::Muted,
            Unixtime::now().unwrap().0,
            Some(&mut txn),
        )?;

        txn.commit()?;

        GLOBALS.ui_people_to_invalidate.write().push(*pubkey);

        Ok(())
    }

    // Returns true if the date passed in is newer than what we already had
    pub(crate) async fn update_relay_list_stamps(
        &self,
        pubkey: PublicKey,
        created_at: i64,
    ) -> Result<bool, Error> {
        let now = Unixtime::now().unwrap().0;

        let mut retval = false;

        let mut person = match GLOBALS.storage.read_person(&pubkey)? {
            Some(person) => person,
            None => Person::new(pubkey),
        };

        person.relay_list_last_received = now;
        if let Some(old_at) = person.relay_list_created_at {
            if created_at < old_at {
                retval = false;
            } else {
                person.relay_list_created_at = Some(created_at);
            }
        } else {
            person.relay_list_created_at = Some(created_at);
        }

        GLOBALS.storage.write_person(&person, None)?;

        Ok(retval)
    }

    pub(crate) async fn update_nip05_last_checked(&self, pubkey: PublicKey) -> Result<(), Error> {
        let now = Unixtime::now().unwrap().0;

        if let Some(mut person) = GLOBALS.storage.read_person(&pubkey)? {
            person.nip05_last_checked = Some(now as u64);
            GLOBALS.storage.write_person(&person, None)?;
        }

        Ok(())
    }

    pub(crate) async fn upsert_nip05_validity(
        &self,
        pubkey: &PublicKey,
        nip05: Option<String>,
        nip05_valid: bool,
        nip05_last_checked: u64,
    ) -> Result<(), Error> {
        // Update memory
        if let Some(mut person) = GLOBALS.storage.read_person(pubkey)? {
            if let Some(metadata) = &mut person.metadata {
                metadata.nip05 = nip05
            } else {
                let mut metadata = Metadata::new();
                metadata.nip05 = nip05;
                person.metadata = Some(metadata);
            }
            person.nip05_valid = nip05_valid;
            person.nip05_last_checked = Some(nip05_last_checked);

            GLOBALS.storage.write_person(&person, None)?;
            GLOBALS.ui_people_to_invalidate.write().push(*pubkey);
        }

        Ok(())
    }

    pub(crate) async fn set_active_person(&self, pubkey: PublicKey) -> Result<(), Error> {
        // Set the active person
        *self.active_person.write().await = Some(pubkey);

        // Load their relays
        let best_relays = GLOBALS.storage.get_best_relays(pubkey, Direction::Write)?;
        *self.active_persons_write_relays.write().await = best_relays;

        Ok(())
    }

    pub fn get_active_person(&self) -> Option<PublicKey> {
        *self.active_person.blocking_read()
    }

    pub async fn get_active_person_async(&self) -> Option<PublicKey> {
        *self.active_person.read().await
    }

    pub fn get_active_person_write_relays(&self) -> Vec<(RelayUrl, u64)> {
        self.active_persons_write_relays.blocking_read().clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Nip05Patch {
    nip05: Option<String>,
}
