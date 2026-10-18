//! Maps stored in git, beside the code they measure, as content-addressed
//! objects: a map is a tree of one blob per function version and one per
//! test's reach, beside a `meta` blob (its format, the source tree it
//! measures, its context, and a BLAKE3 hash of its content). A function
//! unchanged between commits is one blob, stored once.
//!
//! A commit is bound to its map by a git note (on `refs/notes/causal-
//! mutation/CONTEXT`) naming the map's tree; the map records the source
//! tree it measures, which must be the commit's own: anyone can verify it.
//! The pre-commit check writes the map of the tree being committed and
//! names it in `.git/mutamancer-pending`; the post-commit hook binds it
//! to the new commit.

use gix::ObjectId;
use gix::objs::tree::{Entry, EntryKind};
use serde::{Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Store {
    repo: gix::Repository,
}

/// The map's parts, as stored: its meta, and its items' and tests' blobs
/// (each the serialized entry, keyed by its name).
pub struct Parts<M, I, T> {
    pub meta: M,
    pub items: BTreeMap<String, I>,
    pub tests: BTreeMap<String, T>,
}

/// The blob of an entry: its key beside its value, so a tree entry's name
/// (a hash of the key) needs no escaping.
#[derive(Serialize, serde::Deserialize)]
struct Keyed<V> {
    key: String,
    value: V,
}

fn name_of(key: &str) -> String {
    blake3::hash(key.as_bytes()).to_hex()[..32].to_string()
}

impl Store {
    /// The repository containing `dir`.
    pub fn open(dir: &std::path::Path) -> Store {
        let repo = gix::discover(dir).unwrap_or_else(|e| panic!("{}: not in a git repository: {e}", dir.display()));
        Store { repo }
    }

    pub fn git_dir(&self) -> PathBuf {
        self.repo.git_dir().to_path_buf()
    }

    fn blob(&self, bytes: &[u8]) -> ObjectId {
        self.repo.write_blob(bytes).expect("writing a blob").detach()
    }

    /// A tree of `entries` (name, kind, id), in git's order.
    fn tree(&self, mut entries: Vec<(String, EntryKind, ObjectId)>) -> ObjectId {
        // Git orders a subtree as if its name ended in `/`.
        entries.sort_by(|a, b| {
            let key = |(n, k, _): &(String, EntryKind, ObjectId)| if *k == EntryKind::Tree { format!("{n}/") } else { n.clone() };
            key(a).cmp(&key(b))
        });
        let tree = gix::objs::Tree { entries: entries.into_iter().map(|(n, k, id)| Entry { mode: k.into(), filename: n.into(), oid: id }).collect() };
        self.repo.write_object(&tree).expect("writing a tree").detach()
    }

    /// Write a map's parts; the map's tree.
    pub fn write<M: Serialize, I: Serialize, T: Serialize>(&self, parts: &Parts<M, I, T>) -> ObjectId {
        let section = |entries: Vec<(String, Vec<u8>)>| {
            let ids = entries.into_iter().map(|(key, bytes)| (name_of(&key), EntryKind::Blob, self.blob(&bytes))).collect();
            self.tree(ids)
        };
        let items = section(parts.items.iter().map(|(k, v)| (k.clone(), serde_json::to_vec(&Keyed { key: k.clone(), value: v }).unwrap())).collect());
        let tests = section(parts.tests.iter().map(|(k, v)| (k.clone(), serde_json::to_vec(&Keyed { key: k.clone(), value: v }).unwrap())).collect());
        let meta = self.blob(&serde_json::to_vec(&parts.meta).unwrap());
        self.tree(vec![("items".into(), EntryKind::Tree, items), ("meta".into(), EntryKind::Blob, meta), ("tests".into(), EntryKind::Tree, tests)])
    }

    fn entries(&self, tree: ObjectId) -> Vec<(String, ObjectId)> {
        let obj = self.repo.find_object(tree).expect("a map's tree");
        let tree = obj.into_tree();
        tree.decode().expect("decoding a tree").entries.iter().map(|e| (e.filename.to_string(), e.oid.to_owned())).collect()
    }

    fn bytes(&self, id: ObjectId) -> Vec<u8> {
        self.repo.find_object(id).expect("a map's object").data.clone()
    }

    /// Read the map whose tree is `tree`.
    pub fn read<M: DeserializeOwned, I: DeserializeOwned, T: DeserializeOwned>(&self, tree: ObjectId) -> Parts<M, I, T> {
        let top: BTreeMap<String, ObjectId> = self.entries(tree).into_iter().collect();
        let meta = serde_json::from_slice(&self.bytes(top["meta"])).expect("a map's meta");
        let section = |id: ObjectId| -> BTreeMap<String, Vec<u8>> {
            self.entries(id)
                .into_iter()
                .map(|(_, blob)| {
                    let bytes = self.bytes(blob);
                    let k: Keyed<serde::de::IgnoredAny> = serde_json::from_slice(&bytes).expect("an entry");
                    (k.key, bytes)
                })
                .collect()
        };
        Parts { meta, items: decode(section(top["items"])), tests: decode(section(top["tests"])) }
    }

    /// `HEAD`'s commit and its tree, or None in a repository with no
    /// commit yet.
    pub fn head(&self) -> Option<(ObjectId, ObjectId)> {
        let id = self.repo.head_id().ok()?.detach();
        Some((id, self.commit_tree(id)))
    }

    /// The tree of commit `id`.
    pub fn commit_tree(&self, id: ObjectId) -> ObjectId {
        let commit = self.repo.find_object(id).expect("a commit").try_into_commit().expect("a commit");
        commit.tree_id().expect("a commit's tree").detach()
    }

    fn notes_ref(context: &str) -> String {
        format!("refs/notes/mutamancer/{}", &blake3::hash(context.as_bytes()).to_hex()[..16])
    }

    /// The map bound to `commit` in `context`, if any.
    pub fn note(&self, context: &str, commit: ObjectId) -> Option<ObjectId> {
        let r = self.repo.try_find_reference(Self::notes_ref(context).as_str()).ok()??;
        let notes = r.into_fully_peeled_id().ok()?.detach();
        let tree = self.repo.find_object(notes).ok()?.try_into_commit().ok()?.tree_id().ok()?.detach();
        let hex = commit.to_hex().to_string();
        let blob = self.entries(tree).into_iter().find(|(n, _)| *n == hex)?.1;
        ObjectId::from_hex(String::from_utf8(self.bytes(blob)).ok()?.trim().as_bytes()).ok()
    }

    /// Bind `commit` to the map whose tree is `map` in `context`: a new
    /// commit on the notes ref, its tree the previous one with this note.
    pub fn set_note(&self, context: &str, commit: ObjectId, map: ObjectId) {
        let name = Self::notes_ref(context);
        let previous = self.repo.try_find_reference(name.as_str()).ok().flatten().map(|r| r.into_fully_peeled_id().expect("the notes ref's commit").detach());
        let mut entries: Vec<(String, EntryKind, ObjectId)> = match previous {
            Some(p) => {
                let tree = self.repo.find_object(p).unwrap().try_into_commit().unwrap().tree_id().unwrap().detach();
                self.entries(tree).into_iter().map(|(n, id)| (n, EntryKind::Blob, id)).collect()
            }
            None => Vec::new(),
        };
        let hex = commit.to_hex().to_string();
        entries.retain(|(n, _, _)| *n != hex);
        entries.push((hex.clone(), EntryKind::Blob, self.blob(format!("{map}\n").as_bytes())));
        let tree = self.tree(entries);
        let sig = gix::actor::Signature { name: "cargo-mutamancer".into(), email: "".into(), time: gix::date::Time::now_utc() };
        let note = gix::objs::Commit {
            tree,
            parents: previous.into_iter().collect(),
            author: sig.clone(),
            committer: sig,
            encoding: None,
            message: format!("the map of {hex}\n").into(),
            extra_headers: Vec::new(),
        };
        let id = self.repo.write_object(&note).expect("writing the notes commit").detach();
        self.repo.reference(name.as_str(), id, gix::refs::transaction::PreviousValue::Any, "cargo mutamancer: a map bound").expect("updating the notes ref");
    }
}

fn decode<V: DeserializeOwned>(m: BTreeMap<String, Vec<u8>>) -> BTreeMap<String, V> {
    m.into_iter().map(|(k, b)| (k, serde_json::from_slice::<Keyed<V>>(&b).expect("an entry").value)).collect()
}
