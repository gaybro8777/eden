// Copyright 2019 Facebook, Inc.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

mod bfs;
mod cursor;
mod diff;
mod link;
mod store;
#[cfg(test)]
mod testutil;

use std::{
    cmp::Ordering,
    collections::{btree_map::Entry, BTreeMap},
    fmt,
    sync::Arc,
};

use bytes::Bytes;
use crypto::{digest::Digest, sha1::Sha1};
use failure::{bail, Fallible};
use once_cell::sync::OnceCell;

use pathmatcher::{DirectoryMatch, Matcher};
use types::{Key, Node, PathComponent, PathComponentBuf, RepoPath, RepoPathBuf};

pub use self::bfs::BfsDiff;
use self::cursor::{Cursor, Step};
pub use self::diff::{Diff, DiffEntry, DiffType};
use self::link::{Durable, DurableEntry, Ephemeral, Leaf, Link};
use self::store::InnerStore;
pub use self::store::TreeStore;
use crate::{FileMetadata, FsNode, Manifest};

/// The Tree implementation of a Manifest dedicates an inner node for each directory in the
/// repository and a leaf for each file.
#[derive(Clone)]
pub struct Tree {
    store: InnerStore,
    // TODO: root can't be a Leaf
    root: Link,
}

impl Tree {
    /// Instantiates a tree manifest that was stored with the specificed `Node`
    pub fn durable(store: Arc<dyn TreeStore + Send + Sync>, node: Node) -> Self {
        Tree {
            store: InnerStore::new(store),
            root: Link::durable(node),
        }
    }

    /// Instantiates a new tree manifest with no history
    pub fn ephemeral(store: Arc<dyn TreeStore + Send + Sync>) -> Self {
        Tree {
            store: InnerStore::new(store),
            root: Link::Ephemeral(BTreeMap::new()),
        }
    }

    /// Returns an iterator over all the files that are present in the tree.
    pub fn files<'a, M>(&'a self, matcher: &'a M) -> Files<'a, M>
    where
        M: Matcher,
    {
        Files {
            cursor: self.root_cursor(),
            matcher,
        }
    }

    fn root_cursor<'a>(&'a self) -> Cursor<'a> {
        Cursor::new(&self.store, RepoPathBuf::new(), &self.root)
    }
}

impl Manifest for Tree {
    fn get(&self, path: &RepoPath) -> Fallible<Option<FsNode>> {
        let result = self.get_link(path)?.map(|link| {
            if let &Leaf(file_metadata) = link {
                FsNode::File(file_metadata)
            } else {
                FsNode::Directory
            }
        });
        Ok(result)
    }

    fn insert(&mut self, path: RepoPathBuf, file_metadata: FileMetadata) -> Fallible<()> {
        let mut cursor = &self.root;
        let mut must_insert = false;
        for (parent, component) in path.parents().zip(path.components()) {
            let child = match cursor {
                Leaf(_) => bail!(
                    "Asked to insert '{}' but '{}' is already a file.",
                    path,
                    parent
                ),
                Ephemeral(links) => links.get(component),
                Durable(ref entry) => {
                    let links = entry.get_links(&self.store, parent)?;
                    links.get(component)
                }
            };
            match child {
                None => {
                    must_insert = true;
                    break;
                }
                Some(link) => cursor = link,
            }
        }
        if must_insert == false {
            match cursor {
                Leaf(existing_metadata) => {
                    if *existing_metadata == file_metadata {
                        return Ok(()); // nothing to do
                    }
                }
                Ephemeral(_) | Durable(_) => {
                    bail!("Asked to insert '{}' but it is already a directory.", path);
                }
            }
        }
        let (path_parent, last_component) = path.split_last_component().unwrap();
        let mut cursor = &mut self.root;
        // unwrap is fine because root would have been a directory
        for (parent, component) in path_parent.parents().zip(path_parent.components()) {
            cursor = cursor
                .mut_ephemeral_links(&self.store, parent)?
                .entry(component.to_owned())
                .or_insert_with(|| Ephemeral(BTreeMap::new()));
        }
        match cursor
            .mut_ephemeral_links(&self.store, path_parent)?
            .entry(last_component.to_owned())
        {
            Entry::Vacant(entry) => {
                entry.insert(Link::Leaf(file_metadata));
            }
            Entry::Occupied(mut entry) => {
                if let Leaf(ref mut store_ref) = entry.get_mut() {
                    *store_ref = file_metadata;
                } else {
                    unreachable!("Unexpected directory found while insert.");
                }
            }
        }
        Ok(())
    }

    fn remove(&mut self, path: &RepoPath) -> Fallible<Option<FileMetadata>> {
        // The return value lets us know if there are no more files in the subtree and we should be
        // removing it.
        fn do_remove<'a, I>(store: &InnerStore, cursor: &mut Link, iter: &mut I) -> Fallible<bool>
        where
            I: Iterator<Item = (&'a RepoPath, &'a PathComponent)>,
        {
            match iter.next() {
                None => {
                    if let Leaf(_) = cursor {
                        // We reached the file that we want to remove.
                        Ok(true)
                    } else {
                        unreachable!("Unexpected directory found while remove.");
                    }
                }
                Some((parent, component)) => {
                    // TODO: only convert to ephemeral if a removal took place
                    // We are navigating the tree down following parent directories
                    let ephemeral_links = cursor.mut_ephemeral_links(&store, parent)?;
                    // When there is no `component` subtree we behave like the file was removed.
                    if let Some(link) = ephemeral_links.get_mut(component) {
                        if do_remove(store, link, iter)? {
                            // There are no files in the component subtree so we remove it.
                            ephemeral_links.remove(component);
                        }
                    }
                    Ok(ephemeral_links.is_empty())
                }
            }
        }
        if let Some(file_metadata) = self.get_file(path)? {
            do_remove(
                &self.store,
                &mut self.root,
                &mut path.parents().zip(path.components()),
            )?;
            Ok(Some(file_metadata))
        } else {
            Ok(None)
        }
    }

    fn flush(&mut self) -> Fallible<Node> {
        fn compute_node<C: AsRef<[u8]>>(content: C) -> Node {
            let mut hasher = Sha1::new();
            hasher.input(content.as_ref());
            let mut buf = [0u8; Node::len()];
            hasher.result(&mut buf);
            (&buf).into()
        }
        fn do_flush<'a, 'b, 'c>(
            store: &'a InnerStore,
            pathbuf: &'b mut RepoPathBuf,
            cursor: &'c mut Link,
        ) -> Fallible<(&'c Node, store::Flag)> {
            loop {
                match cursor {
                    Leaf(file_metadata) => {
                        return Ok((
                            &file_metadata.node,
                            store::Flag::File(file_metadata.file_type.clone()),
                        ));
                    }
                    Durable(entry) => return Ok((&entry.node, store::Flag::Directory)),
                    Ephemeral(links) => {
                        let iter = links.iter_mut().map(|(component, link)| {
                            pathbuf.push(component.as_path_component());
                            let (node, flag) = do_flush(store, pathbuf, link)?;
                            pathbuf.pop();
                            Ok(store::Element::new(
                                component.to_owned(),
                                node.clone(),
                                flag,
                            ))
                        });
                        let entry = store::Entry::from_elements(iter)?;
                        let node = compute_node(&entry);
                        store.insert_entry(&pathbuf, node, entry)?;

                        let cell = OnceCell::new();
                        // TODO: remove clone
                        cell.set(Ok(links.clone())).unwrap();

                        let durable_entry = DurableEntry { node, links: cell };
                        *cursor = Durable(Arc::new(durable_entry));
                    }
                }
            }
        }
        let mut path = RepoPathBuf::new();
        let (node, _) = do_flush(&self.store, &mut path, &mut self.root)?;
        Ok(node.clone())
    }
}

impl fmt::Debug for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn write_indent(f: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
            write!(f, "{}", str::repeat("| ", indent))?;
            Ok(())
        }
        fn write_children(
            f: &mut fmt::Formatter<'_>,
            children: &BTreeMap<PathComponentBuf, Link>,
            indent: usize,
        ) -> fmt::Result {
            for (component, link) in children {
                write_indent(f, indent)?;
                write!(f, "{} ", component)?;
                write_links(f, link, indent + 1)?;
            }
            Ok(())
        }
        fn write_links(f: &mut fmt::Formatter<'_>, link: &Link, indent: usize) -> fmt::Result {
            match link {
                Link::Leaf(metadata) => {
                    write!(f, "(File, {}, {:?})\n", metadata.node, metadata.file_type)
                }
                Link::Ephemeral(children) => {
                    write!(f, "(Ephemeral)\n")?;
                    write_children(f, children, indent)
                }
                Link::Durable(entry) => {
                    write!(f, "(Durable, {})\n", entry.node)?;
                    match entry.links.get() {
                        None => Ok(()),
                        Some(Err(fallible)) => {
                            write_indent(f, indent)?;
                            write!(f, "failed to load: {:?}", fallible)
                        }
                        Some(Ok(children)) => write_children(f, children, indent),
                    }
                }
            }
        }
        write!(f, "Root ")?;
        write_links(f, &self.root, 1)
    }
}

impl Tree {
    pub fn finalize(
        &mut self,
        parent_trees: Vec<&Tree>,
    ) -> Fallible<impl Iterator<Item = (RepoPathBuf, Node, Bytes, Node, Node)>> {
        fn compute_node<C: AsRef<[u8]>>(parent_tree_nodes: &[Node], content: C) -> Node {
            let mut hasher = Sha1::new();
            debug_assert!(parent_tree_nodes.len() <= 2);
            let p1 = parent_tree_nodes.get(0).unwrap_or(Node::null_id());
            let p2 = parent_tree_nodes.get(1).unwrap_or(Node::null_id());
            // Even if parents are sorted two hashes go into hash computation but surprise
            // the NULL_ID is not a special case in this case and gets sorted.
            if p1 < p2 {
                hasher.input(p1.as_ref());
                hasher.input(p2.as_ref());
            } else {
                hasher.input(p2.as_ref());
                hasher.input(p1.as_ref());
            }
            hasher.input(content.as_ref());
            let mut buf = [0u8; Node::len()];
            hasher.result(&mut buf);
            (&buf).into()
        }
        struct Executor<'a> {
            store: &'a InnerStore,
            path: RepoPathBuf,
            converted_nodes: Vec<(RepoPathBuf, Node, Bytes, Node, Node)>,
            parent_trees: Vec<Cursor<'a>>,
        };
        impl<'a> Executor<'a> {
            fn new(store: &'a InnerStore, parent_trees: &[&'a Tree]) -> Fallible<Executor<'a>> {
                let mut executor = Executor {
                    store,
                    path: RepoPathBuf::new(),
                    converted_nodes: Vec::new(),
                    parent_trees: parent_trees.iter().map(|v| v.root_cursor()).collect(),
                };
                // The first node after step is the root directory. `work()` expects cursors to
                // be pointing to the underlying link.
                for cursor in executor.parent_trees.iter_mut() {
                    match cursor.step() {
                        Step::Success | Step::End => (),
                        Step::Err(err) => return Err(err),
                    }
                }
                Ok(executor)
            }
            fn active_parent_tree_nodes(&self, active_parents: &[usize]) -> Fallible<Vec<Node>> {
                let mut parent_nodes = Vec::with_capacity(active_parents.len());
                for id in active_parents {
                    let cursor = &self.parent_trees[*id];
                    let node = match cursor.link() {
                        Leaf(_) | Ephemeral(_) => unreachable!(),
                        Durable(entry) => entry.node,
                    };
                    parent_nodes.push(node);
                }
                Ok(parent_nodes)
            }
            fn advance_parents(&mut self, active_parents: &[usize]) -> Fallible<()> {
                for id in active_parents {
                    let cursor = &mut self.parent_trees[*id];
                    match cursor.step() {
                        Step::Success | Step::End => (),
                        Step::Err(err) => return Err(err),
                    }
                }
                Ok(())
            }
            fn parent_trees_for_subdirectory(
                &mut self,
                active_parents: &[usize],
            ) -> Fallible<Vec<usize>> {
                let mut result = Vec::new();
                for id in active_parents.iter() {
                    let cursor = &mut self.parent_trees[*id];
                    while !cursor.finished() && cursor.path() < self.path.as_repo_path() {
                        cursor.skip_subtree();
                        match cursor.step() {
                            Step::Success | Step::End => (),
                            Step::Err(err) => return Err(err),
                        }
                    }
                    if !cursor.finished() && cursor.path() == self.path.as_repo_path() {
                        match cursor.link() {
                            Leaf(_) => (), // files and directories don't share history
                            Durable(_) => result.push(*id),
                            Ephemeral(_) => {
                                panic!("Found ephemeral parent when finalizing manifest.")
                            }
                        }
                    }
                }
                Ok(result)
            }
            fn work(
                &mut self,
                link: &mut Link,
                active_parents: Vec<usize>,
            ) -> Fallible<(Node, store::Flag)> {
                let parent_tree_nodes = self.active_parent_tree_nodes(&active_parents)?;
                if let Durable(entry) = link {
                    if parent_tree_nodes.contains(&entry.node) {
                        return Ok((entry.node, store::Flag::Directory));
                    }
                }
                self.advance_parents(&active_parents)?;
                if let Leaf(file_metadata) = link {
                    return Ok((
                        file_metadata.node,
                        store::Flag::File(file_metadata.file_type.clone()),
                    ));
                }
                // TODO: This code is also used on durable nodes for the purpose of generating
                // a list of entries to insert in the local store. For those cases we don't
                // need to convert to Ephemeral instead only verify the hash.
                let links = link.mut_ephemeral_links(self.store, &self.path)?;
                let mut entry = store::EntryMut::new();
                for (component, link) in links.iter_mut() {
                    self.path.push(component.as_path_component());
                    let child_parents = self.parent_trees_for_subdirectory(&active_parents)?;
                    let (node, flag) = self.work(link, child_parents)?;
                    self.path.pop();
                    let element = store::Element::new(component.clone(), node, flag);
                    entry.add_element(element);
                }
                let entry = entry.freeze();
                let node = compute_node(&parent_tree_nodes, &entry);

                let cell = OnceCell::new();
                // TODO: remove clone
                cell.set(Ok(links.clone())).unwrap();

                let durable_entry = DurableEntry { node, links: cell };
                let inner = Arc::new(durable_entry);
                *link = Durable(inner);
                let parent_node = |id| *parent_tree_nodes.get(id).unwrap_or(Node::null_id());
                self.converted_nodes.push((
                    self.path.clone(),
                    node,
                    entry.to_bytes(),
                    parent_node(0),
                    parent_node(1),
                ));
                Ok((node, store::Flag::Directory))
            }
        }

        let mut executor = Executor::new(&self.store, &parent_trees)?;
        executor.work(&mut self.root, (0..parent_trees.len()).collect())?;
        Ok(executor.converted_nodes.into_iter())
    }

    pub fn list(&self, path: &RepoPath) -> Fallible<List> {
        let directory = match self.get_link(path)? {
            None => return Ok(List::NotFound),
            Some(Leaf(_)) => return Ok(List::File),
            Some(Ephemeral(content)) => content,
            Some(Durable(entry)) => entry.get_links(&self.store, path)?,
        };
        Ok(List::Directory(
            directory.keys().map(|key| key.to_owned()).collect(),
        ))
    }

    fn get_link(&self, path: &RepoPath) -> Fallible<Option<&Link>> {
        let mut cursor = &self.root;
        for (parent, component) in path.parents().zip(path.components()) {
            let child = match cursor {
                Leaf(_) => return Ok(None),
                Ephemeral(links) => links.get(component),
                Durable(ref entry) => {
                    let links = entry.get_links(&self.store, parent)?;
                    links.get(component)
                }
            };
            match child {
                None => return Ok(None),
                Some(link) => cursor = link,
            }
        }
        Ok(Some(cursor))
    }
}

pub struct Files<'a, M> {
    cursor: Cursor<'a>,
    matcher: &'a M,
}

impl<'a, M> Iterator for Files<'a, M>
where
    M: Matcher,
{
    type Item = Fallible<(RepoPathBuf, FileMetadata)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.cursor.step() {
                Step::Success => {
                    if let Leaf(file_metadata) = self.cursor.link() {
                        if self.matcher.matches_file(self.cursor.path()) {
                            return Some(Ok((self.cursor.path().to_owned(), *file_metadata)));
                        }
                    } else {
                        if self.matcher.matches_directory(self.cursor.path())
                            == DirectoryMatch::Nothing
                        {
                            self.cursor.skip_subtree();
                        }
                    }
                }
                Step::Err(error) => return Some(Err(error)),
                Step::End => return None,
            }
        }
    }
}

/// Wrapper around `Diff` and `BfsDiff`, allowing the diff algorithm to be dynamically
/// chosen via user configuration.
pub fn diff<'a, M: Matcher>(
    left: &'a Tree,
    right: &'a Tree,
    matcher: &'a M,
    bfs_diff: bool,
) -> Box<dyn Iterator<Item = Fallible<DiffEntry>> + 'a> {
    if bfs_diff {
        Box::new(BfsDiff::new(left, right, matcher))
    } else {
        Box::new(Diff::new(left, right, matcher))
    }
}

#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub enum List {
    NotFound,
    File,
    Directory(Vec<PathComponentBuf>),
}

/// The purpose of this function is to provide compatible behavior with the C++ implementation
/// of the treemanifest. This function is problematic because it goes through abstraction
/// boundaries and is built with the assumption that the storage format is the same as the
/// in memory format that is the same as the wire format.
///
/// This function returns the nodes that need to be sent over the wire for a subtree of the
/// manifest to be fully hydrated. The subtree is represented by `path` and `node`. The data
/// that is present locally by the client is represented by `other_nodes`.
///
/// It is undefined what this function will do when called with a path that points to a file
/// or with nodes that don't make sense.
// NOTE: The implementation is currently custom. Consider converting the code to use Cursor.
// The suggestion received in code review was also to consider making the return type more
// simple (RepoPath, Node) and letting the call sites deal with the Bytes.
pub fn compat_subtree_diff(
    store: Arc<dyn TreeStore + Send + Sync>,
    path: &RepoPath,
    node: Node,
    other_nodes: Vec<Node>,
    depth: i32,
) -> Fallible<Vec<(RepoPathBuf, Node, Bytes)>> {
    struct State {
        store: InnerStore,
        path: RepoPathBuf,
        result: Vec<(RepoPathBuf, Node, Bytes)>,
        depth_remaining: i32,
    }
    impl State {
        fn work(&mut self, node: Node, other_nodes: Vec<Node>) -> Fallible<()> {
            let entry = self.store.get_entry(&self.path, node)?;

            if self.depth_remaining > 0 {
                // TODO: optimize "other_nodes" construction
                // We use BTreeMap for convenience only, it is more efficient to use an array since
                // the entries are already sorted.
                let mut others_map = BTreeMap::new();
                for other_node in other_nodes {
                    let other_entry = self.store.get_entry(&self.path, other_node)?;
                    for other_element_result in other_entry.elements() {
                        let other_element = other_element_result?;
                        others_map
                            .entry(other_element.component)
                            .or_insert(vec![])
                            .push(other_element.node);
                    }
                }
                for element_result in entry.elements() {
                    let element = element_result?;
                    if element.flag != store::Flag::Directory {
                        continue;
                    }
                    let mut others = others_map
                        .remove(&element.component)
                        .unwrap_or_else(|| vec![]);
                    if others.contains(&element.node) {
                        continue;
                    }
                    others.dedup();
                    self.path.push(element.component.as_ref());
                    self.depth_remaining -= 1;
                    self.work(element.node, others)?;
                    self.depth_remaining += 1;
                    self.path.pop();
                }
            }
            // NOTE: order in the result set matters for a lot of the integration tests
            self.result
                .push((self.path.clone(), node, entry.to_bytes()));
            Ok(())
        }
    }

    if other_nodes.contains(&node) {
        return Ok(vec![]);
    }

    let mut state = State {
        store: InnerStore::new(store),
        path: path.to_owned(),
        result: vec![],
        depth_remaining: depth - 1,
    };
    state.work(node, other_nodes)?;
    Ok(state.result)
}

/// A file (leaf node) encountered during a tree traversal.
///
/// Consists of the full path to the file along with the associated file metadata.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct File {
    path: RepoPathBuf,
    meta: FileMetadata,
}

impl File {
    pub(crate) fn new(path: RepoPathBuf, meta: FileMetadata) -> Self {
        Self { path, meta }
    }

    /// Create a file record for a `Link`, failing if the link
    /// refers to a directory rather than a file.
    pub(crate) fn from_link(link: &Link, path: RepoPathBuf) -> Option<Self> {
        match link {
            Link::Leaf(meta) => Some(Self::new(path, *meta)),
            _ => None,
        }
    }

    pub(crate) fn into_left(self) -> DiffEntry {
        DiffEntry::new(self.path, DiffType::LeftOnly(self.meta))
    }

    pub(crate) fn into_right(self) -> DiffEntry {
        DiffEntry::new(self.path, DiffType::RightOnly(self.meta))
    }

    pub(crate) fn into_changed(self, other: File) -> DiffEntry {
        DiffEntry::new(self.path, DiffType::Changed(self.meta, other.meta))
    }
}

/// A directory (inner node) encountered during a tree traversal.
///
/// The directory may have a manifest node hash if it is unmodified from its
/// state on disk. If the directory has in-memory modifications that have not
/// been persisted to disk, it will not have a node hash.
#[derive(Clone, Debug)]
pub(crate) struct Directory<'a> {
    path: RepoPathBuf,
    node: Option<Node>,
    link: &'a Link,
}

impl<'a> Directory<'a> {
    /// Create a directory record for a `Link`, failing if the link
    /// refers to a file rather than a directory.
    pub(crate) fn from_link(link: &'a Link, path: RepoPathBuf) -> Option<Self> {
        let node = match link {
            Link::Leaf(_) => return None,
            Link::Ephemeral(_) => None,
            Link::Durable(entry) => Some(entry.node),
        };
        Some(Self { path, node, link })
    }

    /// Same as `from_link`, but set the directory's path to the empty
    /// path, making this method only useful for the root of the tree.
    pub(crate) fn from_root(link: &'a Link) -> Option<Self> {
        Self::from_link(link, RepoPathBuf::new())
    }

    /// List the contents of this directory.
    ///
    /// Returns two sorted vectors of files and directories contained
    /// in this directory.
    ///
    /// This operation may perform I/O to load the tree entry from the store
    /// if it is not already in memory. Depending on the store implementation,
    /// this may involve an expensive network request if the required data is
    /// not available locally. As such, algorithms that require fast access to
    /// this data should take care to ensure that this content is present
    /// locally before calling this method.
    pub(crate) fn list(&self, store: &InnerStore) -> Fallible<(Vec<File>, Vec<Directory<'a>>)> {
        let mut files = Vec::new();
        let mut dirs = Vec::new();

        let links = match &self.link {
            &Link::Leaf(_) => panic!("programming error: directory cannot be a leaf node"),
            &Link::Ephemeral(ref links) => links,
            &Link::Durable(entry) => entry.get_links(store, &self.path)?,
        };

        for (name, link) in links {
            let mut path = self.path.clone();
            path.push(name.as_ref());
            match link {
                Link::Leaf(_) => {
                    files.push(File::from_link(link, path).expect("leaf node must be a valid file"))
                }
                Link::Ephemeral(_) | Link::Durable(_) => dirs.push(
                    Directory::from_link(link, path).expect("inner node must be a valid directory"),
                ),
            }
        }

        Ok((files, dirs))
    }

    /// Create a `Key` (path/node pair) corresponding to this directory. Keys are used
    /// by the Eden API to fetch data from the server, making this representation useful
    /// for interacting with Mercurial's data fetching code.
    pub(crate) fn key(&self) -> Option<Key> {
        Some(Key::new(self.path.clone(), self.node.clone()?))
    }
}

impl Eq for Directory<'_> {}

impl PartialEq for Directory<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.node == other.node
    }
}

impl Ord for Directory<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.path.cmp(&other.path) {
            Ordering::Equal => self.node.cmp(&other.node),
            ord => ord,
        }
    }
}

impl PartialOrd for Directory<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pathmatcher::{AlwaysMatcher, TreeMatcher};
    use types::{node::NULL_ID, testutil::*};

    use self::{store::TestStore, testutil::*};
    use crate::FileType;

    #[test]
    fn test_insert() {
        let mut tree = Tree::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo/bar"), make_meta("10"))
            .unwrap();
        assert_eq!(
            tree.get_file(repo_path("foo/bar")).unwrap(),
            Some(make_meta("10"))
        );
        assert_eq!(tree.get_file(repo_path("baz")).unwrap(), None);

        tree.insert(repo_path_buf("baz"), make_meta("20")).unwrap();
        assert_eq!(
            tree.get_file(repo_path("foo/bar")).unwrap(),
            Some(make_meta("10"))
        );
        assert_eq!(
            tree.get_file(repo_path("baz")).unwrap(),
            Some(make_meta("20"))
        );

        tree.insert(repo_path_buf("foo/bat"), make_meta("30"))
            .unwrap();
        assert_eq!(
            tree.get_file(repo_path("foo/bat")).unwrap(),
            Some(make_meta("30"))
        );
        assert_eq!(
            tree.get_file(repo_path("foo/bar")).unwrap(),
            Some(make_meta("10"))
        );
        assert_eq!(
            tree.get_file(repo_path("baz")).unwrap(),
            Some(make_meta("20"))
        );

        assert_eq!(
            format!(
                "{}",
                tree.insert(repo_path_buf("foo/bar/error"), make_meta("40"))
                    .unwrap_err()
            ),
            "Asked to insert 'foo/bar/error' but 'foo/bar' is already a file.",
        );
        assert_eq!(
            format!(
                "{}",
                tree.insert(repo_path_buf("foo"), make_meta("50"))
                    .unwrap_err()
            ),
            "Asked to insert 'foo' but it is already a directory.",
        );
    }

    #[test]
    fn test_durable_link() {
        let store = TestStore::new();
        let root_entry = store::Entry::from_elements(vec![
            store_element("foo", "10", store::Flag::Directory),
            store_element("baz", "20", store::Flag::File(FileType::Regular)),
        ])
        .unwrap();
        store
            .insert(RepoPath::empty(), node("1"), root_entry.to_bytes())
            .unwrap();
        let foo_entry = store::Entry::from_elements(vec![store_element(
            "bar",
            "11",
            store::Flag::File(FileType::Regular),
        )])
        .unwrap();
        store
            .insert(repo_path("foo"), node("10"), foo_entry.to_bytes())
            .unwrap();
        let mut tree = Tree::durable(Arc::new(store), node("1"));

        assert_eq!(
            tree.get_file(repo_path("foo/bar")).unwrap(),
            Some(make_meta("11"))
        );
        assert_eq!(
            tree.get_file(repo_path("baz")).unwrap(),
            Some(make_meta("20"))
        );

        tree.insert(repo_path_buf("foo/bat"), make_meta("12"))
            .unwrap();
        assert_eq!(
            tree.get_file(repo_path("foo/bat")).unwrap(),
            Some(make_meta("12"))
        );
        assert_eq!(
            tree.get_file(repo_path("foo/bar")).unwrap(),
            Some(make_meta("11"))
        );
        assert_eq!(
            tree.get_file(repo_path("baz")).unwrap(),
            Some(make_meta("20"))
        );
    }

    #[test]
    fn test_insert_into_directory() {
        let mut tree = Tree::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo/bar/baz"), make_meta("10"))
            .unwrap();
        assert!(tree
            .insert(repo_path_buf("foo/bar"), make_meta("20"))
            .is_err());
        assert!(tree.insert(repo_path_buf("foo"), make_meta("30")).is_err());
    }

    #[test]
    fn test_insert_with_file_parent() {
        let mut tree = Tree::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo"), make_meta("10")).unwrap();
        assert!(tree
            .insert(repo_path_buf("foo/bar"), make_meta("20"))
            .is_err());
        assert!(tree
            .insert(repo_path_buf("foo/bar/baz"), make_meta("30"))
            .is_err());
    }

    #[test]
    fn test_get_from_directory() {
        let mut tree = Tree::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo/bar/baz"), make_meta("10"))
            .unwrap();
        assert_eq!(
            tree.get(repo_path("foo/bar")).unwrap(),
            Some(FsNode::Directory)
        );
        assert_eq!(tree.get(repo_path("foo")).unwrap(), Some(FsNode::Directory));
    }

    #[test]
    fn test_get_with_file_parent() {
        let mut tree = Tree::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo"), make_meta("10")).unwrap();
        assert_eq!(tree.get(repo_path("foo/bar")).unwrap(), None);
        assert_eq!(tree.get(repo_path("foo/bar/baz")).unwrap(), None);
    }

    #[test]
    fn test_remove_from_ephemeral() {
        let mut tree = Tree::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();

        assert_eq!(tree.remove(repo_path("a1")).unwrap(), None);
        assert_eq!(tree.remove(repo_path("a1/b1")).unwrap(), None);
        assert_eq!(tree.remove(repo_path("a1/b1/c1/d1/e1")).unwrap(), None);
        assert_eq!(
            tree.remove(repo_path("a1/b1/c1/d1")).unwrap(),
            Some(make_meta("10"))
        );
        assert_eq!(tree.remove(repo_path("a3")).unwrap(), None);
        assert_eq!(tree.remove(repo_path("a1/b3")).unwrap(), None);
        assert_eq!(tree.remove(repo_path("a1/b1/c1/d2")).unwrap(), None);
        assert_eq!(tree.remove(repo_path("a1/b1/c1/d1/e1")).unwrap(), None);
        assert_eq!(tree.remove(RepoPath::empty()).unwrap(), None);
        assert_eq!(tree.get(repo_path("a1/b1/c1/d1")).unwrap(), None);
        assert_eq!(tree.get(repo_path("a1/b1/c1")).unwrap(), None);
        assert_eq!(
            tree.get(repo_path("a1/b2")).unwrap(),
            Some(FsNode::File(make_meta("20")))
        );
        assert_eq!(
            tree.remove(repo_path("a1/b2")).unwrap(),
            Some(make_meta("20"))
        );
        assert_eq!(tree.get(repo_path("a1")).unwrap(), None);

        assert_eq!(
            tree.get(repo_path("a2/b2/c2")).unwrap(),
            Some(FsNode::File(make_meta("30")))
        );
        assert_eq!(
            tree.remove(repo_path("a2/b2/c2")).unwrap(),
            Some(make_meta("30"))
        );
        assert_eq!(tree.get(repo_path("a2")).unwrap(), None);

        assert_eq!(
            tree.get(RepoPath::empty()).unwrap(),
            Some(FsNode::Directory)
        );
    }

    #[test]
    fn test_remove_from_durable() {
        let store = TestStore::new();
        let root_entry = store::Entry::from_elements(vec![
            store_element("a1", "10", store::Flag::Directory),
            store_element("a2", "20", store::Flag::File(FileType::Regular)),
        ])
        .unwrap();
        store
            .insert(RepoPath::empty(), node("1"), root_entry.to_bytes())
            .unwrap();
        let a1_entry = store::Entry::from_elements(vec![
            store_element("b1", "11", store::Flag::File(FileType::Regular)),
            store_element("b2", "12", store::Flag::File(FileType::Regular)),
        ])
        .unwrap();
        store
            .insert(repo_path("a1"), node("10"), a1_entry.to_bytes())
            .unwrap();
        let mut tree = Tree::durable(Arc::new(store), node("1"));

        assert_eq!(tree.remove(repo_path("a1")).unwrap(), None);
        assert_eq!(
            tree.remove(repo_path("a1/b1")).unwrap(),
            Some(make_meta("11"))
        );
        assert_eq!(tree.get(repo_path("a1/b1")).unwrap(), None);
        assert_eq!(
            tree.get(repo_path("a1/b2")).unwrap(),
            Some(FsNode::File(make_meta("12")))
        );
        assert_eq!(
            tree.remove(repo_path("a1/b2")).unwrap(),
            Some(make_meta("12"))
        );
        assert_eq!(tree.get(repo_path("a1/b2")).unwrap(), None);
        assert_eq!(tree.get(repo_path("a1")).unwrap(), None);
        assert_eq!(tree.get_link(repo_path("a1")).unwrap(), None);

        assert_eq!(
            tree.get(repo_path("a2")).unwrap(),
            Some(FsNode::File(make_meta("20")))
        );
        assert_eq!(tree.remove(repo_path("a2")).unwrap(), Some(make_meta("20")));
        assert_eq!(tree.get(repo_path("a2")).unwrap(), None);

        assert_eq!(
            tree.get(RepoPath::empty()).unwrap(),
            Some(FsNode::Directory)
        );
    }

    #[test]
    fn test_flush() {
        let store = Arc::new(TestStore::new());
        let mut tree = Tree::ephemeral(store.clone());
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();

        let node = tree.flush().unwrap();

        let tree = Tree::durable(store.clone(), node);
        assert_eq!(
            tree.get_file(repo_path("a1/b1/c1/d1")).unwrap(),
            Some(make_meta("10"))
        );
        assert_eq!(
            tree.get_file(repo_path("a1/b2")).unwrap(),
            Some(make_meta("20"))
        );
        assert_eq!(
            tree.get_file(repo_path("a2/b2/c2")).unwrap(),
            Some(make_meta("30"))
        );
        assert_eq!(tree.get(repo_path("a2/b1")).unwrap(), None);
    }

    #[test]
    fn test_finalize_with_zero_and_one_parents() {
        let store = Arc::new(TestStore::new());
        let mut tree = Tree::ephemeral(store.clone());
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();
        let tree_changed: Vec<_> = tree.finalize(vec![]).unwrap().collect();

        assert_eq!(tree_changed.len(), 6);
        assert_eq!(tree_changed[0].0, repo_path_buf("a1/b1/c1"));
        assert_eq!(tree_changed[1].0, repo_path_buf("a1/b1"));
        assert_eq!(tree_changed[2].0, repo_path_buf("a1"));
        assert_eq!(tree_changed[3].0, repo_path_buf("a2/b2"));
        assert_eq!(tree_changed[4].0, repo_path_buf("a2"));
        assert_eq!(tree_changed[5].0, RepoPathBuf::new());

        // we should write before we can update
        // depends on the implementation but it is valid for finalize to query the store
        // for the values returned in the previous finalize call

        use bytes::Bytes;
        for (path, node, raw, _, _) in tree_changed.iter() {
            store.insert(&path, *node, Bytes::from(&raw[..])).unwrap();
        }

        let mut update = tree.clone();
        update
            .insert(repo_path_buf("a1/b2"), make_meta("40"))
            .unwrap();
        update.remove(repo_path("a2/b2/c2")).unwrap();
        update
            .insert(repo_path_buf("a3/b1"), make_meta("50"))
            .unwrap();
        let update_changed: Vec<_> = update.finalize(vec![&tree]).unwrap().collect();
        assert_eq!(update_changed[0].0, repo_path_buf("a1"));
        assert_eq!(update_changed[0].3, tree_changed[2].1);
        assert_eq!(update_changed[0].4, NULL_ID);
        assert_eq!(update_changed[1].0, repo_path_buf("a3"));
        assert_eq!(update_changed[1].3, NULL_ID);
        assert_eq!(update_changed[1].4, NULL_ID);
        assert_eq!(update_changed[2].0, RepoPathBuf::new());
        assert_eq!(update_changed[2].3, tree_changed[5].1);
        assert_eq!(update_changed[2].4, NULL_ID);
    }

    #[test]
    fn test_finalize_merge() {
        let store = Arc::new(TestStore::new());
        let mut p1 = Tree::ephemeral(store.clone());
        p1.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        p1.insert(repo_path_buf("a1/b2"), make_meta("20")).unwrap();
        p1.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();
        let _p1_changed = p1.finalize(vec![]).unwrap();

        let mut p2 = Tree::ephemeral(store.clone());
        p2.insert(repo_path_buf("a1/b2"), make_meta("40")).unwrap();
        p2.insert(repo_path_buf("a3/b1"), make_meta("50")).unwrap();
        let _p2_changed = p2.finalize(vec![]).unwrap();

        let mut tree = p1.clone();
        tree.insert(repo_path_buf("a1/b2"), make_meta("40"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("60"))
            .unwrap();
        tree.insert(repo_path_buf("a3/b1"), make_meta("50"))
            .unwrap();
        let tree_changed: Vec<_> = tree.finalize(vec![&p1, &p2]).unwrap().collect();
        assert_eq!(tree_changed[0].0, repo_path_buf("a1"));
        assert_eq!(tree_changed[0].3, get_node(&p1, repo_path("a1")));
        assert_eq!(tree_changed[0].4, get_node(&p2, repo_path("a1")));

        assert_eq!(tree_changed[1].0, repo_path_buf("a2/b2"));
        assert_eq!(tree_changed[1].3, get_node(&p1, repo_path("a2/b2")));
        assert_eq!(tree_changed[1].4, NULL_ID);
        assert_eq!(tree_changed[2].0, repo_path_buf("a2"));
        assert_eq!(tree_changed[3].0, repo_path_buf("a3"));
        assert_eq!(tree_changed[3].3, get_node(&p2, repo_path("a3")));
        assert_eq!(tree_changed[3].4, NULL_ID);
        assert_eq!(tree_changed[4].0, RepoPathBuf::new());

        assert_eq!(
            vec![tree_changed[4].3, tree_changed[4].4],
            vec![
                get_node(&p1, RepoPath::empty()),
                get_node(&p2, RepoPath::empty()),
            ]
        );
    }

    #[test]
    fn test_finalize_file_to_directory() {
        let store = Arc::new(TestStore::new());
        let mut tree1 = Tree::ephemeral(store.clone());
        tree1.insert(repo_path_buf("a1"), make_meta("10")).unwrap();
        let tree1_changed: Vec<_> = tree1.finalize(vec![]).unwrap().collect();
        assert_eq!(tree1_changed[0].0, RepoPathBuf::new());
        assert_eq!(tree1_changed[0].3, NULL_ID);

        let mut tree2 = Tree::ephemeral(store.clone());
        tree2
            .insert(repo_path_buf("a1/b1"), make_meta("20"))
            .unwrap();
        let tree2_changed: Vec<_> = tree2.finalize(vec![&tree1]).unwrap().collect();
        assert_eq!(tree2_changed[0].0, repo_path_buf("a1"));
        assert_eq!(tree2_changed[0].3, NULL_ID);
        assert_eq!(tree2_changed[1].0, RepoPathBuf::new());
        assert_eq!(tree2_changed[1].3, tree1_changed[0].1);
        assert_eq!(tree2_changed[1].4, NULL_ID);

        let mut tree3 = Tree::ephemeral(store.clone());
        tree3.insert(repo_path_buf("a1"), make_meta("30")).unwrap();
        let tree3_changed: Vec<_> = tree3.finalize(vec![&tree2]).unwrap().collect();
        assert_eq!(tree3_changed[0].0, RepoPathBuf::new());
        assert_eq!(tree3_changed[0].3, tree2_changed[1].1);
        assert_eq!(tree3_changed[0].4, NULL_ID);
    }

    #[test]
    fn test_finalize_on_durable() {
        let store = Arc::new(TestStore::new());
        let mut tree1 = Tree::ephemeral(store.clone());
        tree1
            .insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree1
            .insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree1
            .insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();
        let _tree1_changed = tree1.finalize(vec![]).unwrap();

        let mut tree2 = tree1.clone();
        tree2
            .insert(repo_path_buf("a1/b2"), make_meta("40"))
            .unwrap();
        tree2
            .insert(repo_path_buf("a2/b2/c2"), make_meta("60"))
            .unwrap();
        tree2
            .insert(repo_path_buf("a3/b1"), make_meta("50"))
            .unwrap();
        let tree_changed: Vec<_> = tree2.finalize(vec![&tree1]).unwrap().collect();
        assert_eq!(
            tree2.finalize(vec![&tree1]).unwrap().collect::<Vec<_>>(),
            tree_changed,
        );
    }

    #[test]
    fn test_cursor_skip_on_root() {
        let tree = Tree::ephemeral(Arc::new(TestStore::new()));
        let mut cursor = tree.root_cursor();
        cursor.skip_subtree();
        match cursor.step() {
            Step::Success => panic!("should have reached the end of the tree"),
            Step::End => (), // success
            Step::Err(error) => panic!(error),
        }
    }

    #[test]
    fn test_cursor_skip() {
        fn step<'a>(cursor: &mut Cursor<'a>) {
            match cursor.step() {
                Step::Success => (),
                Step::End => panic!("reached the end too soon"),
                Step::Err(error) => panic!(error),
            }
        }
        let mut tree = Tree::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("a1"), make_meta("10")).unwrap();
        tree.insert(repo_path_buf("a2/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a3"), make_meta("30")).unwrap();

        let mut cursor = tree.root_cursor();
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::empty());
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::from_str("a1").unwrap());
        // Skip leaf
        cursor.skip_subtree();
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::from_str("a2").unwrap());
        // Skip directory
        cursor.skip_subtree();
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::from_str("a3").unwrap());
        // Skip on the element before State::End
        cursor.skip_subtree();
        match cursor.step() {
            Step::Success => panic!("should have reached the end of the tree"),
            Step::End => (), // success
            Step::Err(error) => panic!(error),
        }
    }

    #[test]
    fn test_files_empty() {
        let tree = Tree::ephemeral(Arc::new(TestStore::new()));
        assert!(tree.files(&AlwaysMatcher::new()).next().is_none());
    }

    #[test]
    fn test_files_ephemeral() {
        let mut tree = Tree::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();

        assert_eq!(
            tree.files(&AlwaysMatcher::new())
                .collect::<Fallible<Vec<_>>>()
                .unwrap(),
            vec!(
                (repo_path_buf("a1/b1/c1/d1"), make_meta("10")),
                (repo_path_buf("a1/b2"), make_meta("20")),
                (repo_path_buf("a2/b2/c2"), make_meta("30")),
            )
        );
    }

    #[test]
    fn test_files_durable() {
        let store = Arc::new(TestStore::new());
        let mut tree = Tree::ephemeral(store.clone());
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();
        let node = tree.flush().unwrap();
        let tree = Tree::durable(store.clone(), node);

        assert_eq!(
            tree.files(&AlwaysMatcher::new())
                .collect::<Fallible<Vec<_>>>()
                .unwrap(),
            vec!(
                (repo_path_buf("a1/b1/c1/d1"), make_meta("10")),
                (repo_path_buf("a1/b2"), make_meta("20")),
                (repo_path_buf("a2/b2/c2"), make_meta("30")),
            )
        );
    }

    #[test]
    fn test_files_matcher() {
        let mut tree = Tree::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c3"), make_meta("40"))
            .unwrap();
        tree.insert(repo_path_buf("a3/b2/c3"), make_meta("50"))
            .unwrap();

        assert_eq!(
            tree.files(&TreeMatcher::from_rules(["a2/b2"].iter()))
                .collect::<Fallible<Vec<_>>>()
                .unwrap(),
            vec!(
                (repo_path_buf("a2/b2/c2"), make_meta("30")),
                (repo_path_buf("a2/b2/c3"), make_meta("40"))
            )
        );
        assert_eq!(
            tree.files(&TreeMatcher::from_rules(["a1/*/c1"].iter()))
                .collect::<Fallible<Vec<_>>>()
                .unwrap(),
            vec!((repo_path_buf("a1/b1/c1/d1"), make_meta("10")),)
        );
        assert_eq!(
            tree.files(&TreeMatcher::from_rules(["**/c3"].iter()))
                .collect::<Fallible<Vec<_>>>()
                .unwrap(),
            vec!(
                (repo_path_buf("a2/b2/c3"), make_meta("40")),
                (repo_path_buf("a3/b2/c3"), make_meta("50"))
            )
        );
    }

    #[test]
    fn test_files_finish_on_error_when_collecting_to_vec() {
        let tree = Tree::durable(Arc::new(TestStore::new()), node("1"));
        let file_results = tree.files(&AlwaysMatcher::new()).collect::<Vec<_>>();
        assert_eq!(file_results.len(), 1);
        assert!(file_results[0].is_err());

        let files_result = tree
            .files(&AlwaysMatcher::new())
            .collect::<Result<Vec<_>, _>>();
        assert!(files_result.is_err());
    }

    #[test]
    fn test_debug() {
        use std::fmt::Write;

        let store = Arc::new(TestStore::new());
        let mut tree = Tree::ephemeral(store.clone());
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        let _node = tree.flush().unwrap();

        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();

        let mut output = String::new();
        write!(output, "{:?}", tree).unwrap();
        assert_eq!(
            output,
            "Root (Ephemeral)\n\
             | a1 (Ephemeral)\n\
             | | b1 (Durable, 4f75b40350c5a77ea27d3287b371016e2d940bab)\n\
             | | | c1 (Durable, 4495bc0cc4093ed880fe1eb1489635f3cddcf04d)\n\
             | | | | d1 (File, 0000000000000000000000000000000000000010, Regular)\n\
             | | b2 (File, 0000000000000000000000000000000000000020, Regular)\n\
             | a2 (Ephemeral)\n\
             | | b2 (Ephemeral)\n\
             | | | c2 (File, 0000000000000000000000000000000000000030, Regular)\n\
             "
        );
    }

    #[test]
    fn test_compat_subtree_diff() {
        let store = Arc::new(TestStore::new());
        // add ("", 1), ("foo", 11), ("baz", 21), ("foo/bar", 111)
        let root_1_entry = store::Entry::from_elements(vec![
            store_element("foo", "11", store::Flag::Directory),
            store_element("baz", "21", store::Flag::File(FileType::Regular)),
        ])
        .unwrap();
        store
            .insert(
                RepoPath::empty(),
                node("1"),
                root_1_entry.clone().to_bytes(),
            )
            .unwrap();
        let foo_11_entry = store::Entry::from_elements(vec![store_element(
            "bar",
            "111",
            store::Flag::File(FileType::Regular),
        )])
        .unwrap();
        store
            .insert(
                repo_path("foo"),
                node("11"),
                foo_11_entry.clone().to_bytes(),
            )
            .unwrap();

        // add ("", 2), ("foo", 12), ("baz", 21), ("foo/bar", 112)
        let root_2_entry = store::Entry::from_elements(vec![
            store_element("foo", "12", store::Flag::Directory),
            store_element("baz", "21", store::Flag::File(FileType::Regular)),
        ])
        .unwrap();
        store
            .insert(RepoPath::empty(), node("2"), root_2_entry.to_bytes())
            .unwrap();
        let foo_12_entry = store::Entry::from_elements(vec![store_element(
            "bar",
            "112",
            store::Flag::File(FileType::Regular),
        )])
        .unwrap();
        store
            .insert(repo_path("foo"), node("12"), foo_12_entry.to_bytes())
            .unwrap();

        assert_eq!(
            compat_subtree_diff(
                store.clone(),
                RepoPath::empty(),
                node("1"),
                vec![node("2")],
                3
            )
            .unwrap(),
            vec![
                (
                    repo_path_buf("foo"),
                    node("11"),
                    foo_11_entry.clone().to_bytes()
                ),
                (
                    RepoPathBuf::new(),
                    node("1"),
                    root_1_entry.clone().to_bytes()
                ),
            ]
        );
        assert_eq!(
            compat_subtree_diff(
                store.clone(),
                RepoPath::empty(),
                node("1"),
                vec![node("2")],
                1
            )
            .unwrap(),
            vec![(
                RepoPathBuf::new(),
                node("1"),
                root_1_entry.clone().to_bytes()
            ),]
        );
        assert_eq!(
            compat_subtree_diff(
                store.clone(),
                repo_path("foo"),
                node("11"),
                vec![node("12")],
                3
            )
            .unwrap(),
            vec![(
                repo_path_buf("foo"),
                node("11"),
                foo_11_entry.clone().to_bytes()
            ),]
        );
        assert_eq!(
            compat_subtree_diff(
                store.clone(),
                RepoPath::empty(),
                node("1"),
                vec![node("1")],
                3
            )
            .unwrap(),
            vec![]
        );
        assert_eq!(
            compat_subtree_diff(
                store.clone(),
                repo_path("foo"),
                node("11"),
                vec![node("11")],
                3
            )
            .unwrap(),
            vec![]
        );
        // it is illegal to call compat_subtree_diff with "baz" but we can't validate for it
    }

    #[test]
    fn test_list() {
        let mut tree = Tree::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("a1/b1/c1"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        let _node = tree.flush().unwrap();
        tree.insert(repo_path_buf("a2/b3/c2"), make_meta("30"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b4"), make_meta("30"))
            .unwrap();

        assert_eq!(tree.list(repo_path("not_found")).unwrap(), List::NotFound);
        assert_eq!(tree.list(repo_path("a1/b1/c1")).unwrap(), List::File);
        assert_eq!(
            tree.list(repo_path("a1/b1")).unwrap(),
            List::Directory(vec![path_component_buf("c1")]),
        );
        assert_eq!(
            tree.list(repo_path("a1")).unwrap(),
            List::Directory(vec![path_component_buf("b1"), path_component_buf("b2")]),
        );
        assert_eq!(tree.list(repo_path("a2/b3/c2")).unwrap(), List::File);
        assert_eq!(
            tree.list(repo_path("a2/b3")).unwrap(),
            List::Directory(vec![path_component_buf("c2")]),
        );
        assert_eq!(
            tree.list(repo_path("a2")).unwrap(),
            List::Directory(vec![path_component_buf("b3"), path_component_buf("b4")]),
        );
        assert_eq!(
            tree.list(RepoPath::empty()).unwrap(),
            List::Directory(vec![path_component_buf("a1"), path_component_buf("a2")]),
        );
    }

    #[test]
    fn test_file_from_link() {
        // Leaf link should result in a file.
        let meta = make_meta("a");
        let path = repo_path_buf("test/leaf");

        let leaf = Link::Leaf(meta.clone());
        let file = File::from_link(&leaf, path.clone()).unwrap();

        let expected = File {
            path: path.clone(),
            meta,
        };
        assert_eq!(file, expected);

        // Attempting to use a directory link should fail.
        let ephemeral = Link::ephemeral();
        let _file = File::from_link(&ephemeral, path.clone());;

        // Durable link should result in a directory.
        let durable = Link::durable(node("a"));
        let file = File::from_link(&durable, path.clone());;
        assert!(file.is_none());
    }

    #[test]
    fn test_diff_entry_from_file() {
        let path = repo_path_buf("foo/bar");
        let meta = make_meta("a");
        let file = File {
            path: path.clone(),
            meta: meta.clone(),
        };

        let left = file.clone().into_left();
        let expected = DiffEntry::new(path.clone(), DiffType::LeftOnly(meta.clone()));
        assert_eq!(left, expected);

        let right = file.clone().into_right();
        let expected = DiffEntry::new(path.clone(), DiffType::RightOnly(meta.clone()));
        assert_eq!(right, expected);

        let meta2 = make_meta("b");
        let file2 = File {
            path: path.clone(),
            meta: meta2.clone(),
        };

        let changed = file.into_changed(file2);
        let expected = DiffEntry::new(path, DiffType::Changed(meta, meta2));
        assert_eq!(changed, expected);
    }

    #[test]
    fn test_directory_from_link() {
        let meta = make_meta("a");
        let path = repo_path_buf("test/leaf");

        let ephemeral = Link::ephemeral();
        let dir = Directory::from_link(&ephemeral, path.clone()).unwrap();
        let expected = Directory {
            path: path.clone(),
            node: None,
            link: &ephemeral,
        };
        assert_eq!(dir, expected);

        let hash = node("b");
        let durable = Link::durable(hash);
        let dir = Directory::from_link(&durable, path.clone()).unwrap();
        let expected = Directory {
            path: path.clone(),
            node: Some(hash),
            link: &ephemeral,
        };
        assert_eq!(dir, expected);

        // If the Link is actually a file, we should get None.
        let leaf = Link::Leaf(meta.clone());
        let dir = Directory::from_link(&leaf, path.clone());
        assert!(dir.is_none());
    }

    #[test]
    fn test_list_directory() -> Fallible<()> {
        let tree = make_tree(&[("a", "1"), ("b/f", "2"), ("c", "3"), ("d/f", "4")]);
        let dir = Directory::from_root(&tree.root).unwrap();
        let (files, dirs) = dir.list(&tree.store)?;

        let file_names = files.into_iter().map(|f| f.path).collect::<Vec<_>>();
        let dir_names = dirs.into_iter().map(|d| d.path).collect::<Vec<_>>();

        assert_eq!(file_names, vec![repo_path_buf("a"), repo_path_buf("c")]);
        assert_eq!(dir_names, vec![repo_path_buf("b"), repo_path_buf("d")]);

        Ok(())
    }
}
