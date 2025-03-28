// Copyright 2022 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, sync::Arc};

use futures_signals::signal_vec::MutableVecLockMut;
use indexmap::map::Entry;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::{
    events::{
        reaction::ReactionEventContent,
        relation::{Annotation, Replacement},
        room::{
            encrypted::{self, RoomEncryptedEventContent},
            message::{self, MessageType, RoomMessageEventContent},
            redaction::{
                OriginalSyncRoomRedactionEvent, RoomRedactionEventContent, SyncRoomRedactionEvent,
            },
        },
        AnyMessageLikeEventContent, AnyStateEventContent, AnySyncMessageLikeEvent,
        AnySyncTimelineEvent, BundledRelations, MessageLikeEventType, StateEventType,
    },
    serde::Raw,
    uint, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
};
use tracing::{debug, error, info, warn};

use super::{
    event_item::{BundledReactions, TimelineDetails},
    find_event_by_id, find_event_by_txn_id, find_read_marker, EventTimelineItem, Message,
    TimelineInnerMetadata, TimelineItem, TimelineItemContent, TimelineKey, VirtualTimelineItem,
};
use crate::events::SyncTimelineEventWithoutContent;

pub(super) enum Flow {
    Local {
        txn_id: OwnedTransactionId,
    },
    Remote {
        event_id: OwnedEventId,
        txn_id: Option<OwnedTransactionId>,
        origin_server_ts: MilliSecondsSinceUnixEpoch,
        raw_event: Raw<AnySyncTimelineEvent>,
        position: TimelineItemPosition,
    },
}

impl Flow {
    fn to_key(&self) -> TimelineKey {
        match self {
            Self::Remote { event_id, .. } => TimelineKey::EventId(event_id.to_owned()),
            Self::Local { txn_id } => TimelineKey::TransactionId(txn_id.to_owned()),
        }
    }

    fn origin_server_ts(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        match self {
            Flow::Local { .. } => None,
            Flow::Remote { origin_server_ts, .. } => Some(*origin_server_ts),
        }
    }

    fn raw_event(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        match self {
            Flow::Local { .. } => None,
            Flow::Remote { raw_event, .. } => Some(raw_event),
        }
    }
}

pub(super) struct TimelineEventMetadata {
    pub(super) sender: OwnedUserId,
    pub(super) is_own_event: bool,
    pub(super) relations: Option<BundledRelations>,
    pub(super) encryption_info: Option<EncryptionInfo>,
}

#[derive(Clone)]
pub(super) enum TimelineEventKind {
    Message {
        content: AnyMessageLikeEventContent,
    },
    RedactedMessage,
    Redaction {
        redacts: OwnedEventId,
        content: RoomRedactionEventContent,
    },
    // FIXME: Split further for state keys of different type
    State {
        _content: AnyStateEventContent,
    },
    RedactedState, // AnyRedactedStateEventContent
    FailedToParseMessageLike {
        event_type: MessageLikeEventType,
        error: Arc<serde_json::Error>,
    },
    FailedToParseState {
        event_type: StateEventType,
        state_key: String,
        error: Arc<serde_json::Error>,
    },
}

impl TimelineEventKind {
    pub(super) fn failed_to_parse(
        event: SyncTimelineEventWithoutContent,
        error: serde_json::Error,
    ) -> Self {
        let error = Arc::new(error);
        match event {
            SyncTimelineEventWithoutContent::OriginalMessageLike(ev) => {
                Self::FailedToParseMessageLike { event_type: ev.content.event_type, error }
            }
            SyncTimelineEventWithoutContent::RedactedMessageLike(ev) => {
                Self::FailedToParseMessageLike { event_type: ev.content.event_type, error }
            }
            SyncTimelineEventWithoutContent::OriginalState(ev) => Self::FailedToParseState {
                event_type: ev.content.event_type,
                state_key: ev.state_key,
                error,
            },
            SyncTimelineEventWithoutContent::RedactedState(ev) => Self::FailedToParseState {
                event_type: ev.content.event_type,
                state_key: ev.state_key,
                error,
            },
        }
    }
}

impl From<AnySyncTimelineEvent> for TimelineEventKind {
    fn from(event: AnySyncTimelineEvent) -> Self {
        match event {
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(
                SyncRoomRedactionEvent::Original(OriginalSyncRoomRedactionEvent {
                    redacts,
                    content,
                    ..
                }),
            )) => Self::Redaction { redacts, content },
            AnySyncTimelineEvent::MessageLike(ev) => match ev.original_content() {
                Some(content) => Self::Message { content },
                None => Self::RedactedMessage,
            },
            AnySyncTimelineEvent::State(ev) => match ev.original_content() {
                Some(_content) => Self::State { _content },
                None => Self::RedactedState,
            },
        }
    }
}

pub(super) enum TimelineItemPosition {
    Start,
    End,
    #[cfg(feature = "e2e-encryption")]
    Update(usize),
}

// Bundles together a few things that are needed throughout the different stages
// of handling an event (figuring out whether it should update an existing
// timeline item, transforming that item or creating a new one, updating the
// reactive Vec).
pub(super) struct TimelineEventHandler<'a, 'i> {
    meta: TimelineEventMetadata,
    flow: Flow,
    timeline_items: &'a mut MutableVecLockMut<'i, Arc<TimelineItem>>,
    reaction_map: &'a mut HashMap<TimelineKey, (OwnedUserId, Annotation)>,
    fully_read_event: &'a mut Option<OwnedEventId>,
    fully_read_event_in_timeline: &'a mut bool,
    event_added: bool,
}

impl<'a, 'i> TimelineEventHandler<'a, 'i> {
    pub(super) fn new(
        event_meta: TimelineEventMetadata,
        flow: Flow,
        timeline_items: &'a mut MutableVecLockMut<'i, Arc<TimelineItem>>,
        timeline_meta: &'a mut TimelineInnerMetadata,
    ) -> Self {
        Self {
            meta: event_meta,
            flow,
            timeline_items,
            reaction_map: &mut timeline_meta.reaction_map,
            fully_read_event: &mut timeline_meta.fully_read_event,
            fully_read_event_in_timeline: &mut timeline_meta.fully_read_event_in_timeline,
            event_added: false,
        }
    }

    pub(super) fn handle_event(mut self, event_kind: TimelineEventKind) {
        match event_kind {
            TimelineEventKind::Message { content } => match content {
                AnyMessageLikeEventContent::Reaction(c) => self.handle_reaction(c),
                AnyMessageLikeEventContent::RoomMessage(c) => self.handle_room_message(c),
                AnyMessageLikeEventContent::RoomEncrypted(c) => self.handle_room_encrypted(c),
                // TODO
                _ => {}
            },
            TimelineEventKind::RedactedMessage => {
                self.add(NewEventTimelineItem::redacted_message());
            }
            TimelineEventKind::Redaction { redacts, content } => {
                self.handle_redaction(redacts, content)
            }
            TimelineEventKind::State { .. } | TimelineEventKind::RedactedState => {
                // TODO
            }
            TimelineEventKind::FailedToParseMessageLike { event_type, error } => {
                self.add(NewEventTimelineItem::failed_to_parse_message_like(event_type, error));
            }
            TimelineEventKind::FailedToParseState { event_type, state_key, error } => {
                self.add(NewEventTimelineItem::failed_to_parse_state(event_type, state_key, error));
            }
        }

        if !self.event_added {
            // TODO: Add event as raw
        }
    }

    fn handle_room_message(&mut self, content: RoomMessageEventContent) {
        match content.relates_to {
            Some(message::Relation::Replacement(re)) => {
                self.handle_room_message_edit(re);
            }
            _ => {
                self.add(NewEventTimelineItem::message(content, self.meta.relations.clone()));
            }
        }
    }

    fn handle_room_message_edit(&mut self, replacement: Replacement<MessageType>) {
        let event_id = &replacement.event_id;

        maybe_update_timeline_item(self.timeline_items, event_id, "edit", |item| {
            if self.meta.sender != item.sender() {
                info!(
                    %event_id, original_sender = %item.sender(), edit_sender = %self.meta.sender,
                    "Edit event applies to another user's timeline item, discarding"
                );
                return None;
            }

            let msg = match &item.content {
                TimelineItemContent::Message(msg) => msg,
                TimelineItemContent::RedactedMessage => {
                    info!(%event_id, "Edit event applies to a redacted message, discarding");
                    return None;
                }
                TimelineItemContent::UnableToDecrypt(_) => {
                    info!(
                        %event_id,
                        "Edit event applies to event that couldn't be decrypted, discarding"
                    );
                    return None;
                }
                TimelineItemContent::FailedToParseMessageLike { .. }
                | TimelineItemContent::FailedToParseState { .. } => {
                    info!(
                        %event_id,
                        "Edit event applies to event that couldn't be parsed, discarding"
                    );
                    return None;
                }
            };

            let content = TimelineItemContent::Message(Message {
                msgtype: replacement.new_content,
                in_reply_to: msg.in_reply_to.clone(),
                edited: true,
            });

            Some(item.with_content(content))
        });
    }

    // Redacted reaction events are no-ops so don't need to be handled
    fn handle_reaction(&mut self, c: ReactionEventContent) {
        let event_id: &EventId = &c.relates_to.event_id;

        let items = &mut *self.timeline_items;
        let did_update = maybe_update_timeline_item(items, event_id, "reaction", |item| {
            // Handling of reactions on redacted events is an open question.
            // For now, ignore reactions on redacted events like Element does.
            if let TimelineItemContent::RedactedMessage = item.content {
                debug!(%event_id, "Ignoring reaction on redacted event");
                None
            } else {
                let mut reactions = item.reactions.clone();
                let reaction_details =
                    reactions.bundled.entry(c.relates_to.key.clone()).or_default();

                reaction_details.count += uint!(1);
                if let TimelineDetails::Ready(senders) = &mut reaction_details.senders {
                    senders.push(self.meta.sender.clone());
                }

                Some(item.with_reactions(reactions))
            }
        });

        if did_update {
            self.reaction_map.insert(self.flow.to_key(), (self.meta.sender.clone(), c.relates_to));
        }
    }

    fn handle_room_encrypted(&mut self, c: RoomEncryptedEventContent) {
        match c.relates_to {
            Some(encrypted::Relation::Replacement(_) | encrypted::Relation::Annotation(_)) => {
                // Do nothing for these, as they would not produce a new
                // timeline item when decrypted either
            }
            _ => {
                self.add(NewEventTimelineItem::unable_to_decrypt(c));
            }
        }
    }

    // Redacted redactions are no-ops (unfortunately)
    fn handle_redaction(&mut self, redacts: OwnedEventId, _content: RoomRedactionEventContent) {
        let mut did_update = false;

        if let Some((sender, rel)) =
            self.reaction_map.remove(&TimelineKey::EventId(redacts.clone()))
        {
            let items = &mut *self.timeline_items;
            did_update = maybe_update_timeline_item(items, &rel.event_id, "redaction", |item| {
                let mut reactions = item.reactions.clone();

                let Entry::Occupied(mut details_entry) = reactions.bundled.entry(rel.key) else {
                    return None;
                };
                let details = details_entry.get_mut();
                details.count -= uint!(1);

                if details.count == uint!(0) {
                    details_entry.remove();
                    return Some(item.with_reactions(reactions));
                }

                let TimelineDetails::Ready(senders) = &mut details.senders else {
                    // FIXME: We probably want to support this somehow in
                    //        the future, but right now it's not possible.
                    warn!(
                        "inconsistent state: shouldn't have a reaction_map entry for a \
                            timeline item with incomplete reactions"
                    );
                    return None;
                };

                if let Some(idx) = senders.iter().position(|s| *s == sender) {
                    senders.remove(idx);
                } else {
                    error!(
                        "inconsistent state: sender from reaction_map not in reaction sender list \
                         of timeline item"
                    );
                    return None;
                }

                if u64::from(details.count) != senders.len() as u64 {
                    error!("inconsistent state: reaction count differs from number of senders");
                    // Can't make things worse by updating the item, so no early
                    // return here.
                }

                Some(item.with_reactions(reactions))
            });

            if !did_update {
                warn!("reaction_map out of sync with timeline items");
            }
        }

        // Even if the event being redacted is a reaction (found in
        // `reaction_map`), it can still be present in the timeline items
        // directly with the raw event timeline feature (not yet implemented).
        let items = &mut *self.timeline_items;
        did_update |= update_timeline_item(items, &redacts, "redaction", |item| item.to_redacted());

        if !did_update {
            // We will want to know this when debugging redaction issues.
            debug!(redaction_key = ?self.flow.to_key(), %redacts, "redaction affected no event");
        }
    }

    fn add(&mut self, item: NewEventTimelineItem) {
        self.event_added = true;

        let NewEventTimelineItem { content, reactions } = item;
        let item = EventTimelineItem {
            key: self.flow.to_key(),
            event_id: None,
            sender: self.meta.sender.to_owned(),
            content,
            reactions,
            origin_server_ts: self.flow.origin_server_ts(),
            is_own: self.meta.is_own_event,
            encryption_info: self.meta.encryption_info.clone(),
            raw: self.flow.raw_event().cloned(),
        };

        let item = Arc::new(TimelineItem::Event(item));
        match &self.flow {
            Flow::Local { .. } => {
                self.timeline_items.push_cloned(item);
            }
            Flow::Remote { txn_id, event_id, position, raw_event, .. } => {
                if let Some(txn_id) = txn_id {
                    if let Some((idx, _old_item)) =
                        find_event_by_txn_id(self.timeline_items, txn_id)
                    {
                        // TODO: Check whether anything is different about the
                        //       old and new item?
                        self.timeline_items.set_cloned(idx, item);
                        return;
                    } else {
                        warn!(
                            %txn_id, %event_id,
                            "Received event with transaction ID, but didn't \
                             find matching timeline item"
                        );
                    }
                }

                if let Some((idx, old_item)) = find_event_by_id(self.timeline_items, event_id) {
                    warn!(
                        ?item,
                        ?old_item,
                        raw = raw_event.json().get(),
                        "Received event with an ID we already have a timeline item for"
                    );

                    // With /messages and /sync sometimes disagreeing on order
                    // of messages, we might want to change the position in some
                    // circumstances, but for now this should be good enough.
                    self.timeline_items.set_cloned(idx, item);
                    return;
                }

                match position {
                    TimelineItemPosition::Start => self.timeline_items.insert_cloned(0, item),
                    TimelineItemPosition::End => self.timeline_items.push_cloned(item),
                    #[cfg(feature = "e2e-encryption")]
                    TimelineItemPosition::Update(idx) => self.timeline_items.set_cloned(*idx, item),
                }
            }
        }

        // See if we got the event corresponding to the read marker now.
        if !*self.fully_read_event_in_timeline {
            update_read_marker(
                self.timeline_items,
                self.fully_read_event.as_deref(),
                self.fully_read_event_in_timeline,
            );
        }
    }
}

pub(crate) fn update_read_marker(
    items_lock: &mut MutableVecLockMut<'_, Arc<TimelineItem>>,
    fully_read_event: Option<&EventId>,
    fully_read_event_in_timeline: &mut bool,
) {
    let Some(fully_read_event) = fully_read_event else { return };
    let read_marker_idx = find_read_marker(items_lock);
    let fully_read_event_idx = find_event_by_id(items_lock, fully_read_event).map(|(idx, _)| idx);
    match (read_marker_idx, fully_read_event_idx) {
        (None, None) => {}
        (None, Some(idx)) => {
            *fully_read_event_in_timeline = true;
            let item = TimelineItem::Virtual(VirtualTimelineItem::ReadMarker);
            items_lock.insert_cloned(idx + 1, item.into());
        }
        (Some(_), None) => {
            // Keep the current position of the read marker, hopefully we
            // should have a new position later.
            *fully_read_event_in_timeline = false;
        }
        (Some(from), Some(to)) => {
            *fully_read_event_in_timeline = true;

            // The read marker can't move backwards.
            if from < to {
                items_lock.move_from_to(from, to);
            }
        }
    }
}

/// Returns whether an update happened
fn maybe_update_timeline_item(
    timeline_items: &mut MutableVecLockMut<'_, Arc<TimelineItem>>,
    event_id: &EventId,
    action: &str,
    update: impl FnOnce(&EventTimelineItem) -> Option<EventTimelineItem>,
) -> bool {
    if let Some((idx, item)) = find_event_by_id(timeline_items, event_id) {
        if let Some(new_item) = update(item) {
            timeline_items.set_cloned(idx, Arc::new(TimelineItem::Event(new_item)));
            return true;
        }
    } else {
        debug!(%event_id, "Timeline item not found, discarding {action}");
    }

    false
}

/// Returns whether an update happened
fn update_timeline_item(
    timeline_items: &mut MutableVecLockMut<'_, Arc<TimelineItem>>,
    event_id: &EventId,
    action: &str,
    update: impl FnOnce(&EventTimelineItem) -> EventTimelineItem,
) -> bool {
    maybe_update_timeline_item(timeline_items, event_id, action, move |item| Some(update(item)))
}

struct NewEventTimelineItem {
    content: TimelineItemContent,
    reactions: BundledReactions,
}

impl NewEventTimelineItem {
    // These constructors could also be `From` implementations, but that would
    // allow users to call them directly, which should not be supported
    fn message(c: RoomMessageEventContent, relations: Option<BundledRelations>) -> Self {
        let edited = relations.as_ref().map_or(false, |r| r.replace.is_some());
        let content = TimelineItemContent::Message(Message {
            msgtype: c.msgtype,
            in_reply_to: c.relates_to.and_then(|rel| match rel {
                message::Relation::Reply { in_reply_to } => Some(in_reply_to.event_id),
                _ => None,
            }),
            edited,
        });

        let reactions =
            relations.and_then(|r| r.annotation).map(BundledReactions::from).unwrap_or_default();

        Self { content, reactions }
    }

    fn unable_to_decrypt(content: RoomEncryptedEventContent) -> Self {
        Self::from_content(TimelineItemContent::UnableToDecrypt(content.into()))
    }

    fn redacted_message() -> Self {
        Self::from_content(TimelineItemContent::RedactedMessage)
    }

    fn failed_to_parse_message_like(
        event_type: MessageLikeEventType,
        error: Arc<serde_json::Error>,
    ) -> NewEventTimelineItem {
        Self::from_content(TimelineItemContent::FailedToParseMessageLike { event_type, error })
    }

    fn failed_to_parse_state(
        event_type: StateEventType,
        state_key: String,
        error: Arc<serde_json::Error>,
    ) -> NewEventTimelineItem {
        Self::from_content(TimelineItemContent::FailedToParseState { event_type, state_key, error })
    }

    fn from_content(content: TimelineItemContent) -> Self {
        Self { content, reactions: BundledReactions::default() }
    }
}
