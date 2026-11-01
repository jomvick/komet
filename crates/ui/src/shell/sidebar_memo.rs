//! Memoized sidebar row data. `Shell::render` runs on every notify (loader
//! frames, hover fades), so grouping/sorting/status derivation is keyed on a
//! fingerprint of exactly the inputs it reads and only rebuilt when that changes.

use std::hash::{Hash, Hasher};
use std::rc::Rc;

use chrono::{DateTime, Utc};
use komet_proto::{Chat, ChatIndicator, HarnessId};

use crate::state::AppState;

/// The slice of a `Chat` a sidebar row renders, plus its derived fields.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct SidebarRow {
    pub id: String,
    pub title: Option<String>,
    pub last_activity: DateTime<Utc>,
    pub harness: Option<HarnessId>,
    pub pinned: bool,
    pub status: ChatIndicator,
    pub folder: String,
    pub branch: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct SpaceGroup {
    pub id: String,
    pub name: String,
    pub rows: Vec<SidebarRow>,
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct GroupedRows {
    pub groups: Vec<SpaceGroup>,
    pub projectless: Vec<SidebarRow>,
}

/// Flat (space-filtered) list: rows plus the FLIP key/height order.
#[derive(Debug, Default, PartialEq)]
pub(super) struct FlatRows {
    pub rows: Vec<SidebarRow>,
    pub order: Vec<(String, f32)>,
}

fn row(state: &AppState, chat: &Chat, folder: String, now: DateTime<Utc>) -> SidebarRow {
    let folder = match state.device_name(&chat.device_id) {
        Some(device) => format!("{folder} @ {device}"),
        None => folder,
    };
    SidebarRow {
        id: chat.id.clone(),
        title: chat.title.clone(),
        last_activity: chat.last_message_at.unwrap_or(chat.created_at),
        harness: chat.config.as_ref().map(|c| c.harness),
        pinned: chat.pinned,
        status: state.display_status_for(chat, now),
        folder,
        branch: chat
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_string),
    }
}

pub(super) fn build_grouped(state: &AppState, now: DateTime<Utc>) -> GroupedRows {
    let mut by_space: std::collections::HashMap<&str, Vec<&Chat>> = Default::default();
    let mut orphans: Vec<&Chat> = Vec::new();
    for chat in state.chats.iter().filter(|c| !c.archived) {
        match chat.space_id.as_deref() {
            Some(space_id) => by_space.entry(space_id).or_default().push(chat),
            None => orphans.push(chat),
        }
    }
    let recency = |c: &&Chat| std::cmp::Reverse(c.last_message_at.unwrap_or(c.created_at));
    orphans.sort_by_key(recency);
    let groups = state
        .spaces_sorted()
        .into_iter()
        .map(|space| {
            let mut chats = by_space.remove(space.id.as_str()).unwrap_or_default();
            chats.sort_by_key(recency);
            SpaceGroup {
                id: space.id.clone(),
                name: space.display_name().to_string(),
                rows: chats
                    .into_iter()
                    .map(|c| row(state, c, space.display_name().to_string(), now))
                    .collect(),
            }
        })
        .collect();
    GroupedRows {
        groups,
        projectless: orphans
            .into_iter()
            .map(|c| row(state, c, "~".to_string(), now))
            .collect(),
    }
}

pub(super) fn build_flat(
    state: &AppState,
    filter: Option<&str>,
    now: DateTime<Utc>,
    row_height: f32,
) -> FlatRows {
    let rows: Vec<SidebarRow> = state
        .overview_chats(now)
        .into_iter()
        .filter(|(_, chat)| filter.is_none_or(|id| chat.space_id.as_deref() == Some(id)))
        .map(|(_, chat)| {
            let folder = match (state.space_for_chat(chat), chat.space_id.as_deref()) {
                (Some(space), _) => space.display_name().to_string(),
                (None, None) => "~".to_string(),
                (None, Some(_)) => "?".to_string(),
            };
            row(state, chat, folder, now)
        })
        .collect();
    let order = rows
        .iter()
        .map(|r| (format!("c:{}", r.id), row_height))
        .collect();
    FlatRows { rows, order }
}

/// Hash of every input `build_grouped`/`build_flat` read, including the
/// time-dependent display status (so TTL/staleness flips invalidate it).
pub(super) fn fingerprint(state: &AppState, now: DateTime<Utc>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    state.spaces.len().hash(&mut h);
    for s in &state.spaces {
        s.id.hash(&mut h);
        s.display_name().hash(&mut h);
    }
    state.devices.len().hash(&mut h);
    for d in &state.devices {
        d.id.hash(&mut h);
        d.name.hash(&mut h);
    }
    for c in state.chats.iter().filter(|c| !c.archived) {
        c.id.hash(&mut h);
        c.device_id.hash(&mut h);
        c.space_id.hash(&mut h);
        c.title.hash(&mut h);
        c.pinned.hash(&mut h);
        c.branch.hash(&mut h);
        c.last_message_at.hash(&mut h);
        c.created_at.hash(&mut h);
        c.config.as_ref().map(|c| c.harness).hash(&mut h);
        std::mem::discriminant(&state.display_status_for(c, now)).hash(&mut h);
    }
    h.finish()
}

#[derive(Default)]
pub(super) struct SidebarMemo {
    grouped: Option<(u64, Rc<GroupedRows>)>,
    flat: Option<(u64, Option<String>, Rc<FlatRows>)>,
}

impl SidebarMemo {
    pub fn grouped(&mut self, state: &AppState, now: DateTime<Utc>) -> Rc<GroupedRows> {
        let key = fingerprint(state, now);
        if let Some((k, v)) = &self.grouped
            && *k == key
        {
            return v.clone();
        }
        let v = Rc::new(build_grouped(state, now));
        self.grouped = Some((key, v.clone()));
        v
    }

    pub fn flat(
        &mut self,
        state: &AppState,
        filter: Option<&str>,
        now: DateTime<Utc>,
        row_height: f32,
    ) -> Rc<FlatRows> {
        let key = fingerprint(state, now);
        if let Some((k, f, v)) = &self.flat
            && *k == key
            && f.as_deref() == filter
        {
            return v.clone();
        }
        let v = Rc::new(build_flat(state, filter, now, row_height));
        self.flat = Some((key, filter.map(str::to_string), v.clone()));
        v
    }
}
