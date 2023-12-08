/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::Write;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use blobstore::Blobstore;
use blobstore::BlobstoreGetData;
use bytes::Bytes;
use changeset_info::ChangesetInfo;
use chrono::Local;
use chrono::TimeZone;
use clap::Args;
use clap::ValueEnum;
use cmdlib_displaying::hexdump;
use context::CoreContext;
use git_types::Tree as GitTree;
use mercurial_types::HgChangesetEnvelope;
use mercurial_types::HgFileEnvelope;
use mercurial_types::HgManifestEnvelope;
use mononoke_types::basename_suffix_skeleton_manifest::BasenameSuffixSkeletonManifest;
use mononoke_types::basename_suffix_skeleton_manifest::BssmEntry;
use mononoke_types::basename_suffix_skeleton_manifest_v3::BssmV3Directory;
use mononoke_types::basename_suffix_skeleton_manifest_v3::BssmV3Entry;
use mononoke_types::blame_v2::BlameV2;
use mononoke_types::deleted_manifest_v2::DeletedManifestV2;
use mononoke_types::fastlog_batch::FastlogBatch;
use mononoke_types::fsnode::Fsnode;
use mononoke_types::sharded_map::ShardedMapNode;
use mononoke_types::sharded_map_v2::ShardedMapV2Node;
use mononoke_types::skeleton_manifest::SkeletonManifest;
use mononoke_types::test_manifest::TestManifest;
use mononoke_types::test_sharded_manifest::TestShardedManifest;
use mononoke_types::test_sharded_manifest::TestShardedManifestEntry;
use mononoke_types::typed_hash::DeletedManifestV2Id;
use mononoke_types::unode::FileUnode;
use mononoke_types::unode::ManifestUnode;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ContentAlias;
use mononoke_types::ContentChunk;
use mononoke_types::ContentMetadataV2;
use mononoke_types::FileContents;
use mononoke_types::ThriftConvert;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

#[derive(Args)]
pub struct BlobstoreFetchArgs {
    /// Write raw blob bytes to the given filename instead of
    /// printing to stdout.
    #[clap(long, short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,

    /// Blobstore key to fetch.
    #[clap(required = true)]
    key: String,

    /// Don't show blob info header.
    #[clap(long, short = 'q')]
    quiet: bool,

    /// Decode as a particular type.
    #[clap(long, value_enum, default_value = "auto")]
    decode_as: DecodeAs,
}

#[derive(ValueEnum, Copy, Clone, Eq, PartialEq)]
pub enum DecodeAs {
    Hex,
    Auto,
    Changeset,
    Content,
    ContentChunk,
    HgChangeset,
    HgManifest,
    HgFilenode,
    GitTree,
    SkeletonManifest,
    Fsnode,
    ContentMetadataV2,
    Alias,
    FileUnode,
    ManifestUnode,
    FastlogBatch,
    DeletedManifestV2MapNode,
    DeletedManifestV2,
    BlameV2,
    BasenameSuffixSkeletonManifestMapNode,
    BasenameSuffixSkeletonManifest,
    BasenameSuffixSkeletonManifestV3MapNode,
    BasenameSuffixSkeletonManifestV3,
    ChangesetInfo,
    TestManifest,
    TestShardedManifest,
    TestShardedManifestMapNode,
}

impl DecodeAs {
    fn from_key_prefix(key: &str) -> Option<Self> {
        for index in Some(0)
            .into_iter()
            .chain(key.match_indices('.').map(|(index, _)| index + 1))
        {
            for (prefix, auto_decode_as) in [
                ("changeset.", DecodeAs::Changeset),
                ("content.", DecodeAs::Content),
                ("chunk.", DecodeAs::ContentChunk),
                ("hgchangeset.", DecodeAs::HgChangeset),
                ("hgmanifest.", DecodeAs::HgManifest),
                ("hgfilenode.", DecodeAs::HgFilenode),
                ("git.tree.", DecodeAs::GitTree),
                ("skeletonmanifest.", DecodeAs::SkeletonManifest),
                ("fsnode.", DecodeAs::Fsnode),
                ("content_metadata2.", DecodeAs::ContentMetadataV2),
                ("alias.", DecodeAs::Alias),
                ("fileunode.", DecodeAs::FileUnode),
                ("manifestunode.", DecodeAs::ManifestUnode),
                ("fastlogbatch.", DecodeAs::FastlogBatch),
                (
                    "deletedmanifest2.mapnode.",
                    DecodeAs::DeletedManifestV2MapNode,
                ),
                ("deletedmanifest2.", DecodeAs::DeletedManifestV2),
                ("blame_v2.", DecodeAs::BlameV2),
                (
                    "bssm.mapnode.",
                    DecodeAs::BasenameSuffixSkeletonManifestMapNode,
                ),
                ("bssm.", DecodeAs::BasenameSuffixSkeletonManifest),
                (
                    "bssm3.map2node.",
                    DecodeAs::BasenameSuffixSkeletonManifestV3MapNode,
                ),
                ("bssm3.", DecodeAs::BasenameSuffixSkeletonManifestV3),
                ("testmanifest.", DecodeAs::TestManifest),
                (
                    "testshardedmanifest.map2node.",
                    DecodeAs::TestShardedManifestMapNode,
                ),
                ("testshardedmanifest.", DecodeAs::TestShardedManifest),
                ("changeset_info.", DecodeAs::ChangesetInfo),
            ] {
                if key[index..].starts_with(prefix) {
                    return Some(auto_decode_as);
                }
            }
        }
        None
    }
}

enum Decoded {
    None,
    Fail(String),
    Display(String),
    Hexdump(Bytes),
}

impl Decoded {
    fn try_display<T: std::fmt::Display, E: std::fmt::Display>(data: Result<T, E>) -> Decoded {
        match data {
            Ok(data) => Decoded::Display(data.to_string()),
            Err(err) => Decoded::Fail(err.to_string()),
        }
    }

    fn try_debug<T: std::fmt::Debug, E: std::fmt::Display>(data: Result<T, E>) -> Decoded {
        match data {
            Ok(data) => Decoded::Display(format!("{:#?}", data)),
            Err(err) => Decoded::Fail(err.to_string()),
        }
    }
}

fn decode(key: &str, data: BlobstoreGetData, mut decode_as: DecodeAs) -> Decoded {
    if decode_as == DecodeAs::Auto {
        if let Some(auto_decode_as) = DecodeAs::from_key_prefix(key) {
            decode_as = auto_decode_as;
        }
    }
    match decode_as {
        DecodeAs::Hex | DecodeAs::Auto => Decoded::None,
        DecodeAs::Changeset => Decoded::try_debug(BonsaiChangeset::from_bytes(data.as_raw_bytes())),
        DecodeAs::Content => match FileContents::from_encoded_bytes(data.into_raw_bytes()) {
            Ok(FileContents::Bytes(data)) => Decoded::Hexdump(data),
            Ok(FileContents::Chunked(chunked)) => Decoded::Display(format!("{:#?}", chunked)),
            Err(err) => Decoded::Fail(err.to_string()),
        },
        DecodeAs::ContentChunk => match ContentChunk::from_encoded_bytes(data.into_raw_bytes()) {
            Ok(chunk) => Decoded::Hexdump(chunk.into_bytes()),
            Err(err) => Decoded::Fail(err.to_string()),
        },
        DecodeAs::HgChangeset => Decoded::try_display(HgChangesetEnvelope::from_blob(data.into())),
        DecodeAs::HgManifest => Decoded::try_display(HgManifestEnvelope::from_blob(data.into())),
        DecodeAs::HgFilenode => Decoded::try_display(HgFileEnvelope::from_blob(data.into())),
        DecodeAs::GitTree => Decoded::try_display(GitTree::try_from(data)),
        DecodeAs::SkeletonManifest => {
            Decoded::try_debug(SkeletonManifest::from_bytes(data.into_raw_bytes().as_ref()))
        }
        DecodeAs::Fsnode => Decoded::try_debug(Fsnode::from_bytes(data.into_raw_bytes().as_ref())),
        DecodeAs::ContentMetadataV2 => Decoded::try_debug(ContentMetadataV2::from_bytes(
            data.into_raw_bytes().as_ref(),
        )),
        DecodeAs::Alias => Decoded::try_debug(ContentAlias::from_bytes(data.into_raw_bytes())),
        DecodeAs::FileUnode => {
            Decoded::try_debug(FileUnode::from_bytes(data.into_raw_bytes().as_ref()))
        }
        DecodeAs::ManifestUnode => {
            Decoded::try_debug(ManifestUnode::from_bytes(data.into_raw_bytes().as_ref()))
        }
        DecodeAs::FastlogBatch => {
            Decoded::try_debug(FastlogBatch::from_bytes(&data.into_raw_bytes()))
        }
        DecodeAs::DeletedManifestV2 => {
            Decoded::try_debug(DeletedManifestV2::from_bytes(&data.into_raw_bytes()))
        }
        DecodeAs::DeletedManifestV2MapNode => {
            Decoded::try_debug(ShardedMapNode::<DeletedManifestV2Id>::from_bytes(
                &data.into_raw_bytes(),
            ))
        }
        DecodeAs::BlameV2 => {
            Decoded::try_debug(BlameV2::from_bytes(data.into_raw_bytes().as_ref()))
        }
        DecodeAs::BasenameSuffixSkeletonManifest => Decoded::try_debug(
            BasenameSuffixSkeletonManifest::from_bytes(&data.into_raw_bytes()),
        ),
        DecodeAs::BasenameSuffixSkeletonManifestMapNode => {
            Decoded::try_debug(ShardedMapNode::<BssmEntry>::from_bytes(
                &data.into_raw_bytes(),
            ))
        }
        DecodeAs::BasenameSuffixSkeletonManifestV3 => {
            Decoded::try_debug(BssmV3Directory::from_bytes(&data.into_raw_bytes()))
        }
        DecodeAs::BasenameSuffixSkeletonManifestV3MapNode => Decoded::try_debug(
            ShardedMapV2Node::<BssmV3Entry>::from_bytes(&data.into_raw_bytes()),
        ),
        DecodeAs::ChangesetInfo => {
            Decoded::try_debug(ChangesetInfo::from_bytes(&data.into_raw_bytes()))
        }
        DecodeAs::TestManifest => {
            Decoded::try_debug(TestManifest::from_bytes(&data.into_raw_bytes()))
        }
        DecodeAs::TestShardedManifest => {
            Decoded::try_debug(TestShardedManifest::from_bytes(&data.into_raw_bytes()))
        }
        DecodeAs::TestShardedManifestMapNode => {
            Decoded::try_debug(ShardedMapV2Node::<TestShardedManifestEntry>::from_bytes(
                &data.into_raw_bytes(),
            ))
        }
    }
}

pub async fn fetch(
    ctx: &CoreContext,
    blobstore: &dyn Blobstore,
    fetch_args: BlobstoreFetchArgs,
) -> Result<()> {
    let value = blobstore
        .get(ctx, &fetch_args.key)
        .await
        .context("Failed to fetch blob")?;
    match value {
        None => {
            writeln!(std::io::stderr(), "No blob exists for {}", fetch_args.key)?;
        }
        Some(value) => {
            if !fetch_args.quiet {
                writeln!(std::io::stdout(), "Key: {}", fetch_args.key)?;
                if let Some(ctime) = value.as_meta().ctime() {
                    writeln!(
                        std::io::stdout(),
                        "Ctime: {} ({})",
                        ctime,
                        Local.timestamp_opt(ctime, 0).unwrap()
                    )?;
                }
                if let Some(sizes) = value.as_meta().sizes() {
                    writeln!(
                        std::io::stdout(),
                        "Size: {} ({} compressed)",
                        value.len(),
                        sizes.unique_compressed_size
                    )?;
                } else {
                    writeln!(std::io::stdout(), "Size: {}", value.len())?;
                }
                writeln!(std::io::stdout())?;
            }
            if let Some(output) = fetch_args.output {
                let mut file = File::create(output)
                    .await
                    .context("Failed to create output file")?;
                file.write_all(value.as_raw_bytes())
                    .await
                    .context("Failed to write to output file")?;
                file.flush().await?;
            } else {
                let bytes = value.as_raw_bytes().clone();
                match decode(&fetch_args.key, value, fetch_args.decode_as) {
                    Decoded::Display(decoded) => {
                        writeln!(std::io::stdout(), "{}", decoded)?;
                    }
                    Decoded::Hexdump(data) => {
                        hexdump(std::io::stdout(), data)?;
                    }
                    Decoded::Fail(err) => {
                        writeln!(std::io::stderr(), "Failed to decode: {}", err)?;
                        // Fall back to dumping as raw hex
                        hexdump(std::io::stdout(), bytes)?;
                    }
                    Decoded::None => {
                        hexdump(std::io::stdout(), bytes)?;
                    }
                }
            }
        }
    }

    Ok(())
}
