use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntryMeta {
    pub kind: EntryKind,
    pub size: u64,
    pub modified: i64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub entries: BTreeMap<String, EntryMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanItem {
    Put(String),
    Get(String),
    DeleteLocal(String),
    DeleteRemote(String),
    Conflict(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusItem {
    LocalOnly(String),
    RemoteOnly(String),
    Modified(String),
}

pub fn push_items(local: &Snapshot, remote: &Snapshot, delete: bool) -> Vec<PlanItem> {
    let mut items = Vec::new();
    for (rel, local_meta) in &local.entries {
        if local_meta.kind == EntryKind::File && remote.entries.get(rel) != Some(local_meta) {
            items.push(PlanItem::Put(rel.clone()));
        }
    }
    if delete {
        for rel in remote.entries.keys().rev() {
            if !local.entries.contains_key(rel) {
                items.push(PlanItem::DeleteRemote(rel.clone()));
            }
        }
    }
    items
}

pub fn pull_items(local: &Snapshot, remote: &Snapshot, delete: bool) -> Vec<PlanItem> {
    let mut items = Vec::new();
    for (rel, remote_meta) in &remote.entries {
        if remote_meta.kind == EntryKind::File && local.entries.get(rel) != Some(remote_meta) {
            items.push(PlanItem::Get(rel.clone()));
        }
    }
    if delete {
        for rel in local.entries.keys().rev() {
            if !remote.entries.contains_key(rel) {
                items.push(PlanItem::DeleteLocal(rel.clone()));
            }
        }
    }
    items
}

pub fn sync_items(
    base: &Snapshot,
    local: &Snapshot,
    remote: &Snapshot,
    delete: bool,
) -> Vec<PlanItem> {
    let paths = base
        .entries
        .keys()
        .chain(local.entries.keys())
        .chain(remote.entries.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut items = Vec::new();
    for path in paths {
        let base_meta = base.entries.get(&path);
        let local_meta = local.entries.get(&path);
        let remote_meta = remote.entries.get(&path);
        if local_meta == remote_meta {
            continue;
        }
        let local_changed = local_meta != base_meta;
        let remote_changed = remote_meta != base_meta;
        match (local_changed, remote_changed, local_meta, remote_meta) {
            (true, false, Some(meta), _) if meta.kind == EntryKind::File => {
                items.push(PlanItem::Put(path))
            }
            (true, false, None, _) if delete => items.push(PlanItem::DeleteRemote(path)),
            (false, true, _, Some(meta)) if meta.kind == EntryKind::File => {
                items.push(PlanItem::Get(path))
            }
            (false, true, _, None) if delete => items.push(PlanItem::DeleteLocal(path)),
            (true, true, _, _) => items.push(PlanItem::Conflict(path)),
            _ => {}
        }
    }
    items
}

pub fn status_plan(local: &Snapshot, remote: &Snapshot) -> Vec<StatusItem> {
    let paths = local
        .entries
        .keys()
        .chain(remote.entries.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut items = Vec::new();
    for path in paths {
        match (local.entries.get(&path), remote.entries.get(&path)) {
            (Some(_), None) => items.push(StatusItem::LocalOnly(path)),
            (None, Some(_)) => items.push(StatusItem::RemoteOnly(path)),
            (Some(local), Some(remote)) if local != remote => {
                items.push(StatusItem::Modified(path))
            }
            _ => {}
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(kind: EntryKind, hash: Option<&str>) -> EntryMeta {
        EntryMeta {
            kind,
            size: 1,
            modified: 1,
            sha256: hash.map(str::to_string),
        }
    }

    #[test]
    fn plans_puts_and_deletes() {
        let mut local = Snapshot::default();
        local
            .entries
            .insert("a.txt".to_string(), meta(EntryKind::File, Some("new")));
        let mut remote = Snapshot::default();
        remote
            .entries
            .insert("a.txt".to_string(), meta(EntryKind::File, Some("old")));
        remote
            .entries
            .insert("b.txt".to_string(), meta(EntryKind::File, Some("gone")));
        assert_eq!(
            push_items(&local, &remote, true),
            vec![
                PlanItem::Put("a.txt".to_string()),
                PlanItem::DeleteRemote("b.txt".to_string())
            ]
        );
    }

    #[test]
    fn plans_pull_items() {
        let mut local = Snapshot::default();
        local
            .entries
            .insert("a.txt".to_string(), meta(EntryKind::File, Some("old")));
        local
            .entries
            .insert("gone.txt".to_string(), meta(EntryKind::File, Some("gone")));
        let mut remote = Snapshot::default();
        remote
            .entries
            .insert("a.txt".to_string(), meta(EntryKind::File, Some("new")));
        assert_eq!(
            pull_items(&local, &remote, true),
            vec![
                PlanItem::Get("a.txt".to_string()),
                PlanItem::DeleteLocal("gone.txt".to_string())
            ]
        );
    }

    #[test]
    fn sync_uses_base_to_avoid_clobbering_conflicts() {
        let mut base = Snapshot::default();
        base.entries
            .insert("a.txt".to_string(), meta(EntryKind::File, Some("base")));
        let mut local = Snapshot::default();
        local
            .entries
            .insert("a.txt".to_string(), meta(EntryKind::File, Some("local")));
        let mut remote = Snapshot::default();
        remote
            .entries
            .insert("a.txt".to_string(), meta(EntryKind::File, Some("remote")));
        assert_eq!(
            sync_items(&base, &local, &remote, true),
            vec![PlanItem::Conflict("a.txt".to_string())]
        );
    }

    #[test]
    fn status_reports_all_differences() {
        let mut local = Snapshot::default();
        local
            .entries
            .insert("a.txt".to_string(), meta(EntryKind::File, Some("same")));
        local
            .entries
            .insert("b.txt".to_string(), meta(EntryKind::File, Some("local")));
        let mut remote = Snapshot::default();
        remote
            .entries
            .insert("a.txt".to_string(), meta(EntryKind::File, Some("same")));
        remote
            .entries
            .insert("b.txt".to_string(), meta(EntryKind::File, Some("remote")));
        remote
            .entries
            .insert("c.txt".to_string(), meta(EntryKind::File, Some("remote")));
        assert_eq!(
            status_plan(&local, &remote),
            vec![
                StatusItem::Modified("b.txt".to_string()),
                StatusItem::RemoteOnly("c.txt".to_string())
            ]
        );
    }
}
