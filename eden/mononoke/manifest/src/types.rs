/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::hash::Hash;
use std::hash::Hasher;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::Loadable;
use blobstore::LoadableError;
use blobstore::Storable;
use context::CoreContext;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use mononoke_types::basename_suffix_skeleton_manifest::BasenameSuffixSkeletonManifest;
use mononoke_types::basename_suffix_skeleton_manifest::BssmDirectory;
use mononoke_types::basename_suffix_skeleton_manifest::BssmEntry;
use mononoke_types::basename_suffix_skeleton_manifest_v3::BssmV3Directory;
use mononoke_types::basename_suffix_skeleton_manifest_v3::BssmV3Entry;
use mononoke_types::fsnode::Fsnode;
use mononoke_types::fsnode::FsnodeEntry;
use mononoke_types::fsnode::FsnodeFile;
use mononoke_types::path::MPath;
use mononoke_types::sharded_map::ShardedTrieMap;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::skeleton_manifest::SkeletonManifest;
use mononoke_types::skeleton_manifest::SkeletonManifestEntry;
use mononoke_types::test_manifest::TestManifest;
use mononoke_types::test_manifest::TestManifestDirectory;
use mononoke_types::test_manifest::TestManifestEntry;
use mononoke_types::test_sharded_manifest::TestShardedManifest;
use mononoke_types::test_sharded_manifest::TestShardedManifestDirectory;
use mononoke_types::test_sharded_manifest::TestShardedManifestEntry;
use mononoke_types::unode::ManifestUnode;
use mononoke_types::unode::UnodeEntry;
use mononoke_types::FileUnodeId;
use mononoke_types::FsnodeId;
use mononoke_types::MPathElement;
use mononoke_types::ManifestUnodeId;
use mononoke_types::NonRootMPath;
use mononoke_types::SkeletonManifestId;
use mononoke_types::TrieMap;
use serde_derive::Deserialize;
use serde_derive::Serialize;
use smallvec::SmallVec;

#[async_trait]
pub trait TrieMapOps<Store, Value>: Sized {
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(Option<Value>, Vec<(u8, Self)>)>;

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, Value)>>>;
}

#[async_trait]
impl<Store, V: Send> TrieMapOps<Store, V> for TrieMap<V> {
    async fn expand(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<(Option<V>, Vec<(u8, Self)>)> {
        Ok(self.expand())
    }

    async fn into_stream(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, V)>>> {
        Ok(stream::iter(self).map(Ok).boxed())
    }
}

#[async_trait]
impl<Store: Blobstore> TrieMapOps<Store, Entry<TestShardedManifestDirectory, ()>>
    for LoadableShardedMapV2Node<TestShardedManifestEntry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<TestShardedManifestDirectory, ()>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((entry.map(convert_test_sharded_manifest), children))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(SmallVec<[u8; 24]>, Entry<TestShardedManifestDirectory, ()>)>,
        >,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| (k, convert_test_sharded_manifest(v)))
            .boxed())
    }
}

#[async_trait]
impl<Store: Blobstore> TrieMapOps<Store, Entry<BssmV3Directory, ()>>
    for LoadableShardedMapV2Node<BssmV3Entry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(Option<Entry<BssmV3Directory, ()>>, Vec<(u8, Self)>)> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((entry.map(bssm_v3_to_mf_entry), children))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, Entry<BssmV3Directory, ()>)>>>
    {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| (k, bssm_v3_to_mf_entry(v)))
            .boxed())
    }
}

#[async_trait]
pub trait AsyncManifest<Store: Send + Sync>: Sized + 'static {
    type TreeId: Send + Sync;
    type LeafId: Send + Sync;
    type TrieMapType: Send + Sync;

    async fn list(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::LeafId>)>>>;
    /// List all subentries with a given prefix
    async fn list_prefix(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        prefix: &[u8],
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::LeafId>)>>>;
    async fn lookup(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<Entry<Self::TreeId, Self::LeafId>>>;
    async fn into_trie_map(self, ctx: &CoreContext, blobstore: &Store)
    -> Result<Self::TrieMapType>;
}

pub trait Manifest: Sync + Sized + 'static {
    type TreeId: Send + Sync;
    type LeafId: Send + Sync;
    fn list(&self) -> Box<dyn Iterator<Item = (MPathElement, Entry<Self::TreeId, Self::LeafId>)>>;
    /// List all subentries with a given prefix
    fn list_prefix<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> Box<dyn Iterator<Item = (MPathElement, Entry<Self::TreeId, Self::LeafId>)> + 'a> {
        Box::new(self.list().filter(|(k, _)| k.starts_with(prefix)))
    }
    fn lookup(&self, name: &MPathElement) -> Option<Entry<Self::TreeId, Self::LeafId>>;
}

#[async_trait]
impl<M: Manifest + Send, Store: Send + Sync> AsyncManifest<Store> for M {
    type TreeId = <Self as Manifest>::TreeId;
    type LeafId = <Self as Manifest>::LeafId;
    type TrieMapType = TrieMap<Entry<Self::TreeId, Self::LeafId>>;

    async fn list(
        &self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::LeafId>)>>>
    {
        Ok(stream::iter(Manifest::list(self).map(anyhow::Ok).collect::<Vec<_>>()).boxed())
    }

    async fn list_prefix(
        &self,
        _ctx: &CoreContext,
        _blobstore: &Store,
        prefix: &[u8],
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::LeafId>)>>>
    {
        Ok(stream::iter(
            Manifest::list_prefix(self, prefix)
                .map(anyhow::Ok)
                .collect::<Vec<_>>(),
        )
        .boxed())
    }

    async fn lookup(
        &self,
        _ctx: &CoreContext,
        _blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<Entry<Self::TreeId, Self::LeafId>>> {
        anyhow::Ok(Manifest::lookup(self, name))
    }

    async fn into_trie_map(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<Self::TrieMapType> {
        Ok(Manifest::list(&self).collect())
    }
}

fn to_mf_entry(entry: BssmEntry) -> Entry<BssmDirectory, ()> {
    match entry {
        BssmEntry::Directory(dir) => Entry::Tree(dir),
        BssmEntry::File => Entry::Leaf(()),
    }
}

#[async_trait]
impl<Store: Blobstore> AsyncManifest<Store> for BasenameSuffixSkeletonManifest {
    type TreeId = BssmDirectory;
    type LeafId = ();
    type TrieMapType = ShardedTrieMap<BssmEntry>;

    async fn list(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::LeafId>)>>>
    {
        anyhow::Ok(
            self.clone()
                .into_subentries(ctx, blobstore)
                .map_ok(|(path, entry)| (path, to_mf_entry(entry)))
                .boxed(),
        )
    }

    async fn list_prefix(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        prefix: &[u8],
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::LeafId>)>>>
    {
        anyhow::Ok(
            self.clone()
                .into_prefix_subentries(ctx, blobstore, prefix)
                .map_ok(|(path, entry)| (path, to_mf_entry(entry)))
                .boxed(),
        )
    }

    async fn lookup(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<Entry<Self::TreeId, Self::LeafId>>> {
        Ok(self.lookup(ctx, blobstore, name).await?.map(to_mf_entry))
    }

    async fn into_trie_map(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<Self::TrieMapType> {
        Ok(ShardedTrieMap::new(self.subentries))
    }
}

fn bssm_v3_to_mf_entry(entry: BssmV3Entry) -> Entry<BssmV3Directory, ()> {
    match entry {
        BssmV3Entry::Directory(dir) => Entry::Tree(dir),
        BssmV3Entry::File => Entry::Leaf(()),
    }
}

#[async_trait]
impl<Store: Blobstore> AsyncManifest<Store> for BssmV3Directory {
    type TreeId = BssmV3Directory;
    type LeafId = ();
    type TrieMapType = LoadableShardedMapV2Node<BssmV3Entry>;

    async fn list(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::LeafId>)>>>
    {
        anyhow::Ok(
            self.clone()
                .into_subentries(ctx, blobstore)
                .map_ok(|(path, entry)| (path, bssm_v3_to_mf_entry(entry)))
                .boxed(),
        )
    }

    async fn list_prefix(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        prefix: &[u8],
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::LeafId>)>>>
    {
        anyhow::Ok(
            self.clone()
                .into_prefix_subentries(ctx, blobstore, prefix)
                .map_ok(|(path, entry)| (path, bssm_v3_to_mf_entry(entry)))
                .boxed(),
        )
    }

    async fn lookup(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<Entry<Self::TreeId, Self::LeafId>>> {
        Ok(self
            .lookup(ctx, blobstore, name)
            .await?
            .map(bssm_v3_to_mf_entry))
    }

    async fn into_trie_map(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<Self::TrieMapType> {
        Ok(LoadableShardedMapV2Node::Inlined(self.subentries))
    }
}

#[async_trait]
impl<Store: Blobstore> TrieMapOps<Store, Entry<BssmDirectory, ()>> for ShardedTrieMap<BssmEntry> {
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(Option<Entry<BssmDirectory, ()>>, Vec<(u8, Self)>)> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((entry.map(to_mf_entry), children))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, Entry<BssmDirectory, ()>)>>>
    {
        Ok(self
            .into_stream(ctx, blobstore)
            .await?
            .map_ok(|(path, entry)| (path, to_mf_entry(entry)))
            .boxed())
    }
}

impl Manifest for ManifestUnode {
    type TreeId = ManifestUnodeId;
    type LeafId = FileUnodeId;

    fn lookup(&self, name: &MPathElement) -> Option<Entry<Self::TreeId, Self::LeafId>> {
        self.lookup(name).map(convert_unode)
    }

    fn list(&self) -> Box<dyn Iterator<Item = (MPathElement, Entry<Self::TreeId, Self::LeafId>)>> {
        let v: Vec<_> = self
            .list()
            .map(|(basename, entry)| (basename.clone(), convert_unode(entry)))
            .collect();
        Box::new(v.into_iter())
    }
}

fn convert_unode(unode_entry: &UnodeEntry) -> Entry<ManifestUnodeId, FileUnodeId> {
    match unode_entry {
        UnodeEntry::File(file_unode_id) => Entry::Leaf(file_unode_id.clone()),
        UnodeEntry::Directory(mf_unode_id) => Entry::Tree(mf_unode_id.clone()),
    }
}

impl Manifest for Fsnode {
    type TreeId = FsnodeId;
    type LeafId = FsnodeFile;

    fn lookup(&self, name: &MPathElement) -> Option<Entry<Self::TreeId, Self::LeafId>> {
        self.lookup(name).map(convert_fsnode)
    }

    fn list(&self) -> Box<dyn Iterator<Item = (MPathElement, Entry<Self::TreeId, Self::LeafId>)>> {
        let v: Vec<_> = self
            .list()
            .map(|(basename, entry)| (basename.clone(), convert_fsnode(entry)))
            .collect();
        Box::new(v.into_iter())
    }
}

fn convert_fsnode(fsnode_entry: &FsnodeEntry) -> Entry<FsnodeId, FsnodeFile> {
    match fsnode_entry {
        FsnodeEntry::File(fsnode_file) => Entry::Leaf(*fsnode_file),
        FsnodeEntry::Directory(fsnode_directory) => Entry::Tree(fsnode_directory.id().clone()),
    }
}

impl Manifest for SkeletonManifest {
    type TreeId = SkeletonManifestId;
    type LeafId = ();

    fn lookup(&self, name: &MPathElement) -> Option<Entry<Self::TreeId, Self::LeafId>> {
        self.lookup(name).map(convert_skeleton_manifest)
    }

    fn list(&self) -> Box<dyn Iterator<Item = (MPathElement, Entry<Self::TreeId, Self::LeafId>)>> {
        let v: Vec<_> = self
            .list()
            .map(|(basename, entry)| (basename.clone(), convert_skeleton_manifest(entry)))
            .collect();
        Box::new(v.into_iter())
    }
}

fn convert_skeleton_manifest(
    skeleton_entry: &SkeletonManifestEntry,
) -> Entry<SkeletonManifestId, ()> {
    match skeleton_entry {
        SkeletonManifestEntry::File => Entry::Leaf(()),
        SkeletonManifestEntry::Directory(skeleton_directory) => {
            Entry::Tree(skeleton_directory.id().clone())
        }
    }
}

impl Manifest for TestManifest {
    type TreeId = TestManifestDirectory;
    type LeafId = ();

    fn lookup(&self, name: &MPathElement) -> Option<Entry<Self::TreeId, Self::LeafId>> {
        self.lookup(name).map(convert_test_manifest)
    }

    fn list(&self) -> Box<dyn Iterator<Item = (MPathElement, Entry<Self::TreeId, Self::LeafId>)>> {
        let v: Vec<_> = self
            .list()
            .map(|(basename, entry)| (basename.clone(), convert_test_manifest(entry)))
            .collect();
        Box::new(v.into_iter())
    }
}

fn convert_test_manifest(
    test_manifest_entry: &TestManifestEntry,
) -> Entry<TestManifestDirectory, ()> {
    match test_manifest_entry {
        TestManifestEntry::File => Entry::Leaf(()),
        TestManifestEntry::Directory(dir) => Entry::Tree(dir.clone()),
    }
}

#[async_trait]
impl<Store: Blobstore> AsyncManifest<Store> for TestShardedManifest {
    type TreeId = TestShardedManifestDirectory;
    type LeafId = ();
    type TrieMapType = LoadableShardedMapV2Node<TestShardedManifestEntry>;

    async fn list(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::LeafId>)>>>
    {
        anyhow::Ok(
            self.clone()
                .into_subentries(ctx, blobstore)
                .map_ok(|(path, entry)| (path, convert_test_sharded_manifest(entry)))
                .boxed(),
        )
    }

    async fn list_prefix(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        prefix: &[u8],
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::LeafId>)>>>
    {
        anyhow::Ok(
            self.clone()
                .into_prefix_subentries(ctx, blobstore, prefix)
                .map_ok(|(path, entry)| (path, convert_test_sharded_manifest(entry)))
                .boxed(),
        )
    }

    async fn lookup(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<Entry<Self::TreeId, Self::LeafId>>> {
        Ok(self
            .lookup(ctx, blobstore, name)
            .await?
            .map(convert_test_sharded_manifest))
    }

    async fn into_trie_map(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<Self::TrieMapType> {
        Ok(LoadableShardedMapV2Node::Inlined(self.subentries))
    }
}

fn convert_test_sharded_manifest(
    test_sharded_manifest_entry: TestShardedManifestEntry,
) -> Entry<TestShardedManifestDirectory, ()> {
    match test_sharded_manifest_entry {
        TestShardedManifestEntry::File(_file) => Entry::Leaf(()),
        TestShardedManifestEntry::Directory(dir) => Entry::Tree(dir),
    }
}

pub type Weight = usize;

pub trait OrderedManifest: Manifest {
    fn lookup_weighted(
        &self,
        name: &MPathElement,
    ) -> Option<Entry<(Weight, <Self as Manifest>::TreeId), <Self as Manifest>::LeafId>>;
    fn list_weighted(
        &self,
    ) -> Box<
        dyn Iterator<
            Item = (
                MPathElement,
                Entry<(Weight, <Self as Manifest>::TreeId), <Self as Manifest>::LeafId>,
            ),
        >,
    >;
}

#[async_trait]
pub trait AsyncOrderedManifest<Store: Send + Sync>: AsyncManifest<Store> {
    async fn list_weighted(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(MPathElement, Entry<(Weight, Self::TreeId), Self::LeafId>)>,
        >,
    >;
    async fn lookup_weighted(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<Entry<(Weight, Self::TreeId), Self::LeafId>>>;
}

#[async_trait]
impl<M: OrderedManifest + Send, Store: Send + Sync> AsyncOrderedManifest<Store> for M {
    async fn list_weighted(
        &self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(MPathElement, Entry<(Weight, Self::TreeId), Self::LeafId>)>,
        >,
    > {
        Ok(stream::iter(
            OrderedManifest::list_weighted(self)
                .map(anyhow::Ok)
                .collect::<Vec<_>>(),
        )
        .boxed())
    }
    async fn lookup_weighted(
        &self,
        _ctx: &CoreContext,
        _blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<Entry<(Weight, Self::TreeId), Self::LeafId>>> {
        anyhow::Ok(OrderedManifest::lookup_weighted(self, name))
    }
}

fn convert_bssm_to_weighted(entry: Entry<BssmDirectory, ()>) -> Entry<(Weight, BssmDirectory), ()> {
    match entry {
        Entry::Tree(dir) => Entry::Tree((dir.rollup_count.try_into().unwrap_or(usize::MAX), dir)),
        Entry::Leaf(()) => Entry::Leaf(()),
    }
}

#[async_trait]
impl<Store: Blobstore> AsyncOrderedManifest<Store> for BasenameSuffixSkeletonManifest {
    async fn list_weighted(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(MPathElement, Entry<(Weight, Self::TreeId), Self::LeafId>)>,
        >,
    > {
        self.list(ctx, blobstore).await.map(|stream| {
            stream
                .map_ok(|(p, entry)| (p, convert_bssm_to_weighted(entry)))
                .boxed()
        })
    }

    async fn lookup_weighted(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<Entry<(Weight, Self::TreeId), Self::LeafId>>> {
        AsyncManifest::lookup(self, ctx, blobstore, name)
            .await
            .map(|opt| opt.map(convert_bssm_to_weighted))
    }
}

impl OrderedManifest for SkeletonManifest {
    fn lookup_weighted(
        &self,
        name: &MPathElement,
    ) -> Option<Entry<(Weight, <Self as Manifest>::TreeId), <Self as Manifest>::LeafId>> {
        self.lookup(name).map(convert_skeleton_manifest_weighted)
    }

    fn list_weighted(
        &self,
    ) -> Box<
        dyn Iterator<
            Item = (
                MPathElement,
                Entry<(Weight, <Self as Manifest>::TreeId), <Self as Manifest>::LeafId>,
            ),
        >,
    > {
        let v: Vec<_> = self
            .list()
            .map(|(basename, entry)| (basename.clone(), convert_skeleton_manifest_weighted(entry)))
            .collect();
        Box::new(v.into_iter())
    }
}

fn convert_skeleton_manifest_weighted(
    skeleton_entry: &SkeletonManifestEntry,
) -> Entry<(Weight, SkeletonManifestId), ()> {
    match skeleton_entry {
        SkeletonManifestEntry::File => Entry::Leaf(()),
        SkeletonManifestEntry::Directory(skeleton_directory) => {
            let summary = skeleton_directory.summary();
            let weight = summary.descendant_files_count + summary.descendant_dirs_count;
            Entry::Tree((weight as Weight, skeleton_directory.id().clone()))
        }
    }
}

impl OrderedManifest for Fsnode {
    fn lookup_weighted(
        &self,
        name: &MPathElement,
    ) -> Option<Entry<(Weight, <Self as Manifest>::TreeId), <Self as Manifest>::LeafId>> {
        self.lookup(name).map(convert_fsnode_weighted)
    }

    fn list_weighted(
        &self,
    ) -> Box<
        dyn Iterator<
            Item = (
                MPathElement,
                Entry<(Weight, <Self as Manifest>::TreeId), <Self as Manifest>::LeafId>,
            ),
        >,
    > {
        let v: Vec<_> = self
            .list()
            .map(|(basename, entry)| (basename.clone(), convert_fsnode_weighted(entry)))
            .collect();
        Box::new(v.into_iter())
    }
}

fn convert_fsnode_weighted(fsnode_entry: &FsnodeEntry) -> Entry<(Weight, FsnodeId), FsnodeFile> {
    match fsnode_entry {
        FsnodeEntry::File(fsnode_file) => Entry::Leaf(*fsnode_file),
        FsnodeEntry::Directory(fsnode_directory) => {
            let summary = fsnode_directory.summary();
            // Fsnodes don't have a full descendant dirs count, so we use the
            // child count as a lower-bound estimate.
            let weight = summary.descendant_files_count + summary.child_dirs_count;
            Entry::Tree((weight as Weight, fsnode_directory.id().clone()))
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Entry<T, L> {
    Tree(T),
    Leaf(L),
}

impl<T, L> Entry<T, L> {
    pub fn into_tree(self) -> Option<T> {
        match self {
            Entry::Tree(tree) => Some(tree),
            _ => None,
        }
    }

    pub fn into_leaf(self) -> Option<L> {
        match self {
            Entry::Leaf(leaf) => Some(leaf),
            _ => None,
        }
    }

    pub fn map_leaf<L2>(self, m: impl FnOnce(L) -> L2) -> Entry<T, L2> {
        match self {
            Entry::Tree(tree) => Entry::Tree(tree),
            Entry::Leaf(leaf) => Entry::Leaf(m(leaf)),
        }
    }

    pub fn map_tree<T2>(self, m: impl FnOnce(T) -> T2) -> Entry<T2, L> {
        match self {
            Entry::Tree(tree) => Entry::Tree(m(tree)),
            Entry::Leaf(leaf) => Entry::Leaf(leaf),
        }
    }

    pub fn is_tree(&self) -> bool {
        match self {
            Entry::Tree(_) => true,
            _ => false,
        }
    }
}

#[async_trait]
impl<T, L> Loadable for Entry<T, L>
where
    T: Loadable + Sync,
    L: Loadable + Sync,
{
    type Value = Entry<T::Value, L::Value>;

    async fn load<'a, B: Blobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError> {
        Ok(match self {
            Entry::Tree(tree_id) => Entry::Tree(tree_id.load(ctx, blobstore).await?),
            Entry::Leaf(leaf_id) => Entry::Leaf(leaf_id.load(ctx, blobstore).await?),
        })
    }
}

#[async_trait]
impl<T, L> Storable for Entry<T, L>
where
    T: Storable + Send,
    L: Storable + Send,
{
    type Key = Entry<T::Key, L::Key>;

    async fn store<'a, B: Blobstore>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Key> {
        Ok(match self {
            Entry::Tree(tree) => Entry::Tree(tree.store(ctx, blobstore).await?),
            Entry::Leaf(leaf) => Entry::Leaf(leaf.store(ctx, blobstore).await?),
        })
    }
}

#[derive(Clone, Debug)]
pub struct PathTree<V> {
    pub value: V,
    pub subentries: TrieMap<Self>,
}

impl<V> PathTree<V> {
    pub fn deconstruct(self) -> (V, Vec<(MPathElement, Self)>) {
        (
            self.value,
            self.subentries
                .into_iter()
                .map(|(path, subtree)| {
                    (
                        MPathElement::from_smallvec(path)
                            .expect("Only MPaths are inserted into PathTree"),
                        subtree,
                    )
                })
                .collect(),
        )
    }

    pub fn get(&self, path: &MPath) -> Option<&V> {
        let mut tree = self;
        for elem in path {
            match tree.subentries.get(elem.as_ref()) {
                Some(subtree) => tree = subtree,
                None => return None,
            }
        }
        Some(&tree.value)
    }
}

impl<V> PathTree<V>
where
    V: Default,
{
    pub fn insert(&mut self, path: MPath, value: V) {
        let node = path.into_iter().fold(self, |node, element| {
            node.subentries.get_or_insert_default(element)
        });
        node.value = value;
    }

    pub fn insert_and_merge<T>(&mut self, path: MPath, value: T)
    where
        V: Extend<T>,
    {
        let node = path.into_iter().fold(self, |node, element| {
            node.subentries.get_or_insert_default(element)
        });
        node.value.extend(std::iter::once(value));
    }

    pub fn insert_and_prune(&mut self, path: MPath, value: V) {
        let node = path.into_iter().fold(self, |node, element| {
            node.subentries.get_or_insert_default(element)
        });
        node.value = value;
        node.subentries.clear();
    }
}

impl<V> Default for PathTree<V>
where
    V: Default,
{
    fn default() -> Self {
        Self {
            value: Default::default(),
            subentries: Default::default(),
        }
    }
}

impl<V> FromIterator<(MPath, V)> for PathTree<V>
where
    V: Default,
{
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (MPath, V)>,
    {
        let mut tree: Self = Default::default();
        for (path, value) in iter {
            tree.insert(path, value);
        }
        tree
    }
}

impl<V> FromIterator<(NonRootMPath, V)> for PathTree<V>
where
    V: Default,
{
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (NonRootMPath, V)>,
    {
        let mut tree: Self = Default::default();
        for (path, value) in iter {
            tree.insert(MPath::from(path), value);
        }
        tree
    }
}

pub struct PathTreeIter<V> {
    frames: Vec<(MPath, PathTree<V>)>,
}

impl<V> Iterator for PathTreeIter<V> {
    type Item = (MPath, V);

    fn next(&mut self) -> Option<Self::Item> {
        let (path, path_tree) = self.frames.pop()?;
        let (value, subentries) = path_tree.deconstruct();

        for (name, subentry) in subentries {
            self.frames.push((path.join(&name), subentry));
        }
        Some((path, value))
    }
}

impl<V> IntoIterator for PathTree<V> {
    type Item = (MPath, V);
    type IntoIter = PathTreeIter<V>;

    fn into_iter(self) -> Self::IntoIter {
        PathTreeIter {
            frames: vec![(MPath::ROOT, self)],
        }
    }
}

/// Traced allows you to trace a given parent through manifest derivation. For example, if you
/// assign ID 1 to a tree, then perform manifest derivation, then further entries you presented to
/// you that came from this parent will have the same ID.
#[derive(Debug)]
pub struct Traced<I, E>(Option<I>, E);

impl<I, E: Hash> Hash for Traced<I, E> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.1.hash(state);
    }
}

impl<I, E: PartialEq> PartialEq for Traced<I, E> {
    fn eq(&self, other: &Self) -> bool {
        self.1 == other.1
    }
}

impl<I, E: Eq> Eq for Traced<I, E> {}

impl<I: Copy, E: Copy> Copy for Traced<I, E> {}

impl<I: Clone, E: Clone> Clone for Traced<I, E> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), self.1.clone())
    }
}

impl<I, E> Traced<I, E> {
    pub fn generate(e: E) -> Self {
        Self(None, e)
    }

    pub fn assign(i: I, e: E) -> Self {
        Self(Some(i), e)
    }

    pub fn id(&self) -> Option<&I> {
        self.0.as_ref()
    }

    pub fn untraced(&self) -> &E {
        &self.1
    }

    pub fn into_untraced(self) -> E {
        self.1
    }
}

impl<I: Copy, E> Traced<I, E> {
    fn inherit_into_entry<TreeId, LeafId>(
        &self,
        e: Entry<TreeId, LeafId>,
    ) -> Entry<Traced<I, TreeId>, Traced<I, LeafId>> {
        match e {
            Entry::Tree(t) => Entry::Tree(Traced(self.0, t)),
            Entry::Leaf(l) => Entry::Leaf(Traced(self.0, l)),
        }
    }
}

impl<I, TreeId, LeafId> From<Entry<Traced<I, TreeId>, Traced<I, LeafId>>>
    for Entry<TreeId, LeafId>
{
    fn from(entry: Entry<Traced<I, TreeId>, Traced<I, LeafId>>) -> Self {
        match entry {
            Entry::Tree(Traced(_, t)) => Entry::Tree(t),
            Entry::Leaf(Traced(_, l)) => Entry::Leaf(l),
        }
    }
}

impl<I: Send + Sync + Copy + 'static, M: Manifest> Manifest for Traced<I, M> {
    type TreeId = Traced<I, <M as Manifest>::TreeId>;
    type LeafId = Traced<I, <M as Manifest>::LeafId>;

    fn list(&self) -> Box<dyn Iterator<Item = (MPathElement, Entry<Self::TreeId, Self::LeafId>)>> {
        Box::new(
            self.1
                .list()
                .map(|(path, entry)| (path, self.inherit_into_entry(entry)))
                .collect::<Vec<_>>()
                .into_iter(),
        )
    }

    fn lookup(&self, name: &MPathElement) -> Option<Entry<Self::TreeId, Self::LeafId>> {
        self.1.lookup(name).map(|e| self.inherit_into_entry(e))
    }
}

#[async_trait]
impl<I: Clone + 'static + Send + Sync, M: Loadable + Send + Sync> Loadable for Traced<I, M> {
    type Value = Traced<I, <M as Loadable>::Value>;

    async fn load<'a, B: Blobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError> {
        let id = self.0.clone();
        let v = self.1.load(ctx, blobstore).await?;
        Ok(Traced(id, v))
    }
}
