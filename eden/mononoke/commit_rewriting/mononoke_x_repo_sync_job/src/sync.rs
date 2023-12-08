/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::format_err;
use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use blobstore::Loadable;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLogEntry;
use bookmarks::BookmarkUpdateReason;
use bookmarks::BookmarksRef;
use changeset_fetcher::ChangesetFetcherArc;
use changeset_fetcher::ChangesetFetcherRef;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use cross_repo_sync::find_toposorted_unsynced_ancestors;
use cross_repo_sync::types::Source;
use cross_repo_sync::types::Target;
use cross_repo_sync::CandidateSelectionHint;
use cross_repo_sync::CommitSyncContext;
use cross_repo_sync::CommitSyncOutcome;
use cross_repo_sync::CommitSyncer;
use cross_repo_sync::PushrebaseRewriteDates;
use futures::future::try_join_all;
use futures::stream::TryStreamExt;
use futures::try_join;
use futures_stats::TimedFutureExt;
use metaconfig_types::CommitSyncConfigVersion;
use mononoke_types::ChangesetId;
use mononoke_types::Timestamp;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentityRef;
use scuba_ext::MononokeScubaSampleBuilder;
use slog::info;
use slog::warn;
use synced_commit_mapping::SyncedCommitMapping;

use crate::reporting::log_bookmark_deletion_result;
use crate::reporting::log_non_pushrebase_sync_single_changeset_result;
use crate::reporting::log_pushrebase_sync_single_changeset_result;

pub trait Repo = cross_repo_sync::Repo;

#[derive(Debug, Eq, PartialEq)]
pub enum SyncResult {
    Synced(Vec<ChangesetId>),
    // SkippedNoKnownVersion usually happens when a new root commit was
    // added to the repository, and its descendant are not merged into any
    // mainline bookmark. See top level doc comments in main file for
    // more details.
    SkippedNoKnownVersion,
}

/// Sync all new commits and update the bookmark that were introduced by BookmarkUpdateLogEntry
/// in the source repo.
/// This function:
/// 1) Finds commits that needs syncing
/// 2) Syncs them from source repo into target (*)
/// 3) Updates the bookmark
///
/// (*) There are two ways how a commit can be synced from source repo into a target repo.
/// It can either be rewritten and saved into a target repo, or rewritten and pushrebased
/// in a target repo. This depends on which bookmark introduced a commit - if it's a
/// common_pushrebase_bookmark (usually "master"), then a commit will be pushrebased.
/// Otherwise it will be synced without pushrebase.
pub async fn sync_single_bookmark_update_log<M: SyncedCommitMapping + Clone + 'static, R>(
    ctx: &CoreContext,
    commit_syncer: &CommitSyncer<M, R>,
    entry: BookmarkUpdateLogEntry,
    common_pushrebase_bookmarks: &HashSet<BookmarkKey>,
    mut scuba_sample: MononokeScubaSampleBuilder,
    pushrebase_rewrite_dates: PushrebaseRewriteDates,
) -> Result<SyncResult, Error>
where
    R: Repo,
{
    info!(ctx.logger(), "processing log entry #{}", entry.id);
    let source_bookmark = Source(entry.bookmark_name);
    let target_bookmark = Target(
        commit_syncer.get_bookmark_renamer().await?(&source_bookmark)
            .ok_or_else(|| format_err!("unexpected empty bookmark rename"))?,
    );
    scuba_sample
        .add("source_bookmark_name", format!("{}", source_bookmark))
        .add("target_bookmark_name", format!("{}", target_bookmark));

    let to_cs_id = match entry.to_changeset_id {
        Some(to_cs_id) => to_cs_id,
        None => {
            // This is a bookmark deletion - just delete a bookmark and exit,
            // no need to sync commits
            process_bookmark_deletion(
                ctx,
                commit_syncer,
                scuba_sample,
                &source_bookmark,
                &target_bookmark,
                common_pushrebase_bookmarks,
                Some(entry.timestamp),
            )
            .await?;

            return Ok(SyncResult::Synced(vec![]));
        }
    };

    sync_commit_and_ancestors(
        ctx,
        commit_syncer,
        entry.from_changeset_id,
        to_cs_id,
        &Some(target_bookmark),
        common_pushrebase_bookmarks,
        scuba_sample,
        pushrebase_rewrite_dates,
        Some(entry.timestamp),
    )
    .await
    // TODO(stash): test with other movers
    // Note: counter update might fail after a successful sync
}

/// Sync and all of its unsynced ancestors **if the given commit has at least
/// one synced ancestor**.
pub async fn sync_commit_and_ancestors<M: SyncedCommitMapping + Clone + 'static, R>(
    ctx: &CoreContext,
    commit_syncer: &CommitSyncer<M, R>,
    from_cs_id: Option<ChangesetId>,
    to_cs_id: ChangesetId,
    target_bookmark: &Option<Target<BookmarkKey>>,
    common_pushrebase_bookmarks: &HashSet<BookmarkKey>,
    scuba_sample: MononokeScubaSampleBuilder,
    pushrebase_rewrite_dates: PushrebaseRewriteDates,
    bookmark_update_timestamp: Option<Timestamp>,
) -> Result<SyncResult, Error>
where
    R: Repo,
{
    let (unsynced_ancestors, unsynced_ancestors_versions) =
        find_toposorted_unsynced_ancestors(ctx, commit_syncer, to_cs_id.clone()).await?;

    let version = if !unsynced_ancestors_versions.has_ancestor_with_a_known_outcome() {
        return Ok(SyncResult::SkippedNoKnownVersion);
    } else {
        let maybe_version = unsynced_ancestors_versions
            .get_only_version()
            .with_context(|| format!("failed to backsync cs id {}", to_cs_id))?;
        maybe_version.ok_or_else(|| {
            format_err!(
                "failed to sync {} - all of the ancestors are NotSyncCandidate",
                to_cs_id
            )
        })?
    };

    let len = unsynced_ancestors.len();
    info!(ctx.logger(), "{} unsynced ancestors of {}", len, to_cs_id);

    if let Some(target_bookmark) = target_bookmark {
        // This is forward sync. The direction is small to large, so the source bookmark is the small
        // bookmark which is the key in the common_pushrebase_bookmarks
        // Source: small, e.g. `heads/main`
        // Target: large, e.g. `main`
        // common_pushrebase_bookmarks: large, e.g. `["main"]`

        if common_pushrebase_bookmarks.contains(target_bookmark) {
            // This is a commit that was introduced by common pushrebase bookmark (e.g. "master").
            // Use pushrebase to sync a commit.
            if let Some(from_cs_id) = from_cs_id {
                check_forward_move(ctx, commit_syncer, to_cs_id, from_cs_id).await?;
            }

            return sync_commits_via_pushrebase(
                ctx,
                commit_syncer,
                target_bookmark,
                common_pushrebase_bookmarks,
                scuba_sample.clone(),
                unsynced_ancestors,
                &version,
                pushrebase_rewrite_dates,
                bookmark_update_timestamp,
            )
            .await
            .map(SyncResult::Synced);
        }
    }
    // Use a normal sync since a bookmark is not a common pushrebase bookmark
    let mut res = vec![];
    for cs_id in unsynced_ancestors {
        let synced = sync_commit_without_pushrebase(
            ctx,
            commit_syncer,
            scuba_sample.clone(),
            cs_id,
            common_pushrebase_bookmarks,
            &version,
            bookmark_update_timestamp,
        )
        .await?;
        res.extend(synced);
    }
    let maybe_remapped_cs_id = find_remapped_cs_id(ctx, commit_syncer, to_cs_id).await?;
    let remapped_cs_id =
        maybe_remapped_cs_id.ok_or_else(|| format_err!("unknown sync outcome for {}", to_cs_id))?;
    if let Some(target_bookmark) = target_bookmark {
        move_or_create_bookmark(
            ctx,
            commit_syncer.get_target_repo(),
            target_bookmark,
            remapped_cs_id,
        )
        .await?;
    }
    Ok(SyncResult::Synced(res))
}

/// This function syncs commits via pushrebase with a caveat - some commits shouldn't be
/// pushrebased! Consider pushing of a merge
///
/// ```text
///  source repo (X - synced commit, O - unsynced commit)
///
///     O <- merge commit (this commit needs to be pushrebased in target repo)
///    / |
///   X   O <- this commit DOES NOT NEED to be pushrebased in the target repo
///  ...  |
///      ...
///
/// Just as normal pushrebase behaves while pushing merges, we rebase the actual merge
/// commit and it's ancestors, but we don't rebase merge ancestors.
/// ```
pub async fn sync_commits_via_pushrebase<M: SyncedCommitMapping + Clone + 'static, R>(
    ctx: &CoreContext,
    commit_syncer: &CommitSyncer<M, R>,
    target_bookmark: &Target<BookmarkKey>,
    common_pushrebase_bookmarks: &HashSet<BookmarkKey>,
    scuba_sample: MononokeScubaSampleBuilder,
    unsynced_ancestors: Vec<ChangesetId>,
    version: &CommitSyncConfigVersion,
    pushrebase_rewrite_dates: PushrebaseRewriteDates,
    bookmark_update_timestamp: Option<Timestamp>,
) -> Result<Vec<ChangesetId>, Error>
where
    R: Repo,
{
    let source_repo = commit_syncer.get_source_repo();
    // It stores commits that were introduced as part of current bookmark update, but that
    // shouldn't be pushrebased.
    let mut no_pushrebase = HashSet::new();
    let mut res = vec![];

    // Iterate in reverse order i.e. descendants before ancestors
    for cs_id in unsynced_ancestors.iter().rev() {
        if no_pushrebase.contains(cs_id) {
            continue;
        }

        let bcs = cs_id.load(ctx, source_repo.repo_blobstore()).await?;

        let mut parents = bcs.parents();
        let maybe_p1 = parents.next();
        let maybe_p2 = parents.next();
        if let (Some(p1), Some(p2)) = (maybe_p1, maybe_p2) {
            if parents.next().is_some() {
                return Err(format_err!("only 2 parent merges are supported"));
            }

            no_pushrebase.extend(validate_if_new_repo_merge(ctx, source_repo, p1, p2).await?);
        }
    }

    for cs_id in unsynced_ancestors {
        let maybe_new_cs_id = if no_pushrebase.contains(&cs_id) {
            sync_commit_without_pushrebase(
                ctx,
                commit_syncer,
                scuba_sample.clone(),
                cs_id,
                common_pushrebase_bookmarks,
                version,
                bookmark_update_timestamp,
            )
            .await?
        } else {
            info!(
                ctx.logger(),
                "syncing {} via pushrebase for {}", cs_id, &target_bookmark
            );
            let (stats, result) = pushrebase_commit(
                ctx,
                commit_syncer,
                target_bookmark,
                cs_id,
                pushrebase_rewrite_dates,
            )
            .timed()
            .await;
            log_pushrebase_sync_single_changeset_result(
                ctx.clone(),
                scuba_sample.clone(),
                cs_id,
                &result,
                stats,
                bookmark_update_timestamp,
            );
            let maybe_new_cs_id = result?;
            maybe_new_cs_id.into_iter().collect()
        };

        res.extend(maybe_new_cs_id);
    }
    Ok(res)
}

pub async fn sync_commit_without_pushrebase<M: SyncedCommitMapping + Clone + 'static, R>(
    ctx: &CoreContext,
    commit_syncer: &CommitSyncer<M, R>,
    scuba_sample: MononokeScubaSampleBuilder,
    cs_id: ChangesetId,
    common_pushrebase_bookmarks: &HashSet<BookmarkKey>,
    version: &CommitSyncConfigVersion,
    bookmark_update_timestamp: Option<Timestamp>,
) -> Result<Vec<ChangesetId>, Error>
where
    R: Repo,
{
    info!(ctx.logger(), "syncing {}", cs_id);
    let bcs = cs_id
        .load(ctx, commit_syncer.get_source_repo().repo_blobstore())
        .await?;

    let (stats, result) = if bcs.is_merge() {
        // We allow syncing of a merge only if there's no intersection between ancestors of this
        // merge commit and ancestors of common pushrebase bookmark in target repo.
        // The code below does exactly that - it fetches common_pushrebase_bookmarks and parent
        // commits from the target repo, and then it checks if there are no intersection.
        let target_repo = commit_syncer.get_target_repo();
        let mut book_values = vec![];
        for common_bookmark in common_pushrebase_bookmarks {
            book_values.push(target_repo.bookmarks().get(ctx.clone(), common_bookmark));
        }

        let book_values = try_join_all(book_values).await?;
        let book_values = book_values.into_iter().flatten().collect();

        let parents = try_join_all(
            bcs.parents()
                .map(|p| find_remapped_cs_id(ctx, commit_syncer, p)),
        )
        .await?;
        let maybe_independent_branch = check_if_independent_branch_and_return(
            ctx,
            target_repo,
            parents.into_iter().flatten().collect(),
            book_values,
        )
        .await?;

        // Merge is from a branch completely independent from common_pushrebase_bookmark -
        // it's fine to sync it.
        if maybe_independent_branch.is_none() {
            bail!(
                "cannot sync merge commit - one of it's ancestors is an ancestor of a common pushrebase bookmark"
            );
        };

        commit_syncer
            .unsafe_always_rewrite_sync_commit(
                ctx,
                cs_id,
                None,
                version,
                CommitSyncContext::XRepoSyncJob,
            )
            .timed()
            .await
    } else {
        commit_syncer
            .unsafe_sync_commit(
                ctx,
                cs_id,
                CandidateSelectionHint::Only,
                CommitSyncContext::XRepoSyncJob,
                Some(version.clone()),
            )
            .timed()
            .await
    };

    log_non_pushrebase_sync_single_changeset_result(
        ctx.clone(),
        scuba_sample.clone(),
        cs_id,
        &result,
        stats,
        bookmark_update_timestamp,
    );

    let maybe_cs_id = result?;
    Ok(maybe_cs_id.into_iter().collect())
}

/// Run the initial import of a small repo into a large repo.
/// It will sync a specific commit (i.e. head commit) and all of its ancestors
/// and optionally bookmark the head commit.
pub async fn sync_commits_for_initial_import<M: SyncedCommitMapping + Clone + 'static, R>(
    ctx: &CoreContext,
    commit_syncer: &CommitSyncer<M, R>,
    scuba_sample: MononokeScubaSampleBuilder,
    // Head commit to sync. All of its unsynced ancestors will be synced as well.
    cs_id: ChangesetId,
    // Sync config version to use for importing the commits.
    config_version: CommitSyncConfigVersion,
) -> Result<Vec<ChangesetId>>
where
    R: Repo,
{
    info!(ctx.logger(), "syncing {}", cs_id);

    let (unsynced_ancestors, _unsynced_ancestors_versions) =
        find_toposorted_unsynced_ancestors(ctx, commit_syncer, cs_id.clone()).await?;

    let mut res = vec![];
    // Sync all of the ancestors first
    for ancestor_cs_id in unsynced_ancestors {
        let mb_synced = commit_syncer
            .unsafe_sync_commit(
                ctx,
                ancestor_cs_id,
                CandidateSelectionHint::Only,
                CommitSyncContext::ForwardSyncerInitialImport,
                Some(config_version.clone()),
            )
            .await?;
        let synced =
            mb_synced.ok_or(anyhow!("Failed to sync ancestor commit {}", ancestor_cs_id))?;
        res.push(synced);
    }

    let (stats, result) = commit_syncer
        .unsafe_sync_commit(
            ctx,
            cs_id,
            CandidateSelectionHint::Only,
            CommitSyncContext::ForwardSyncerInitialImport,
            Some(config_version),
        )
        .timed()
        .await;

    let maybe_cs_id: Option<ChangesetId> = result?;

    // Check that the head commit was synced properly and log something otherwise
    // clippy: This warning relates to creating `err` as `Err(...)` followed by `unwrap_err()`
    // below, which would be redundant.
    // In this instance, it ignores the fact that `err` is used in between by a function that needs
    // a borrow to a `Result`.
    // Since the `Result` owns its content, trying to work around it forces a clone which feels
    // worse than muting clippy for this instance.
    #[allow(clippy::unnecessary_literal_unwrap)]
    let new_cs_id = maybe_cs_id.ok_or_else(|| {
        let err = Err(anyhow!("Head changeset wasn't synced"));
        log_non_pushrebase_sync_single_changeset_result(
            ctx.clone(),
            scuba_sample.clone(),
            cs_id,
            &err,
            stats.clone(),
            None,
        );
        err.unwrap_err()
    })?;

    res.push(new_cs_id.clone());

    log_non_pushrebase_sync_single_changeset_result(
        ctx.clone(),
        scuba_sample,
        cs_id,
        &Ok(Some(new_cs_id)),
        stats,
        None,
    );
    Ok(res)
}

async fn process_bookmark_deletion<M: SyncedCommitMapping + Clone + 'static, R>(
    ctx: &CoreContext,
    commit_syncer: &CommitSyncer<M, R>,
    scuba_sample: MononokeScubaSampleBuilder,
    source_bookmark: &Source<BookmarkKey>,
    target_bookmark: &Target<BookmarkKey>,
    common_pushrebase_bookmarks: &HashSet<BookmarkKey>,
    bookmark_update_timestamp: Option<Timestamp>,
) -> Result<(), Error>
where
    R: Repo,
{
    if common_pushrebase_bookmarks.contains(source_bookmark) {
        Err(format_err!(
            "unexpected deletion of a shared bookmark {}",
            source_bookmark
        ))
    } else {
        info!(ctx.logger(), "deleting bookmark {}", target_bookmark);
        let (stats, result) = delete_bookmark(
            ctx.clone(),
            commit_syncer.get_target_repo(),
            target_bookmark,
        )
        .timed()
        .await;
        log_bookmark_deletion_result(scuba_sample, &result, stats, bookmark_update_timestamp);
        result
    }
}

async fn check_forward_move<M: SyncedCommitMapping + Clone + 'static, R>(
    ctx: &CoreContext,
    commit_syncer: &CommitSyncer<M, R>,
    to_cs_id: ChangesetId,
    from_cs_id: ChangesetId,
) -> Result<(), Error>
where
    R: Repo,
{
    if !commit_syncer
        .get_source_repo()
        .commit_graph()
        .is_ancestor(ctx, from_cs_id, to_cs_id)
        .await?
    {
        return Err(format_err!(
            "non-forward moves of shared bookmarks are not allowed"
        ));
    }
    Ok(())
}

async fn find_remapped_cs_id<M: SyncedCommitMapping + Clone + 'static, R>(
    ctx: &CoreContext,
    commit_syncer: &CommitSyncer<M, R>,
    orig_cs_id: ChangesetId,
) -> Result<Option<ChangesetId>, Error>
where
    R: Repo,
{
    let maybe_sync_outcome = commit_syncer
        .get_commit_sync_outcome(ctx, orig_cs_id)
        .await?;
    use CommitSyncOutcome::*;
    match maybe_sync_outcome {
        Some(RewrittenAs(cs_id, _)) | Some(EquivalentWorkingCopyAncestor(cs_id, _)) => {
            Ok(Some(cs_id))
        }
        Some(NotSyncCandidate(_)) => Err(format_err!("unexpected NotSyncCandidate")),
        None => Ok(None),
    }
}

async fn pushrebase_commit<M: SyncedCommitMapping + Clone + 'static, R>(
    ctx: &CoreContext,
    commit_syncer: &CommitSyncer<M, R>,
    target_bookmark: &Target<BookmarkKey>,
    cs_id: ChangesetId,
    pushrebase_rewrite_dates: PushrebaseRewriteDates,
) -> Result<Option<ChangesetId>, Error>
where
    R: Repo,
{
    let source_repo = commit_syncer.get_source_repo();
    let bcs = cs_id.load(ctx, source_repo.repo_blobstore()).await?;
    commit_syncer
        .unsafe_sync_commit_pushrebase(
            ctx,
            bcs,
            target_bookmark.clone(),
            CommitSyncContext::XRepoSyncJob,
            pushrebase_rewrite_dates,
        )
        .await
}

/// Function validates if a this merge is supported for x-repo sync. At the moment we support
/// only a single type of merges - merge that introduces a new repo i.e. merge p1 and p2
/// have no shared history.
///
///     O <- merge commit to sync
///    / |
///   O   O <- these are new commits we need to sync
///   |   |
///   |   ...
///
/// This function returns new commits that were introduced by this merge
async fn validate_if_new_repo_merge(
    ctx: &CoreContext,
    repo: &(
         impl ChangesetFetcherRef
         + ChangesetFetcherArc
         + RepoBlobstoreRef
         + RepoIdentityRef
         + CommitGraphRef
     ),
    p1: ChangesetId,
    p2: ChangesetId,
) -> Result<Vec<ChangesetId>, Error> {
    let p1gen = repo.changeset_fetcher().get_generation_number(ctx, p1);
    let p2gen = repo.changeset_fetcher().get_generation_number(ctx, p2);
    let (p1gen, p2gen) = try_join!(p1gen, p2gen)?;
    // FIXME: this code has an assumption that parent with a smaller generation number is a
    // parent that introduces a new repo. This is usually the case, however it might not be true
    // in some rare cases.
    let (larger_gen, smaller_gen) = if p1gen > p2gen { (p1, p2) } else { (p2, p1) };

    let err_msg = || format_err!("unsupported merge - only merges of new repos are supported");

    // Check if this is a diamond merge i.e. check if any of the ancestor of smaller_gen
    // is also ancestor of larger_gen.
    let maybe_independent_branch =
        check_if_independent_branch_and_return(ctx, repo, vec![smaller_gen], vec![larger_gen])
            .await?;

    let independent_branch = maybe_independent_branch.ok_or_else(err_msg)?;

    Ok(independent_branch)
}

/// Checks if `branch_tips` and their ancestors have no intersection with ancestors of
/// other_branches. If there are no intersection then branch_tip and it's ancestors are returned,
/// i.e. (::branch_tips) is returned in mercurial's revset terms
async fn check_if_independent_branch_and_return(
    ctx: &CoreContext,
    repo: &(impl ChangesetFetcherArc + RepoBlobstoreRef + RepoIdentityRef + CommitGraphRef),
    branch_tips: Vec<ChangesetId>,
    other_branches: Vec<ChangesetId>,
) -> Result<Option<Vec<ChangesetId>>, Error> {
    let blobstore = repo.repo_blobstore();
    let bcss = repo
        .commit_graph()
        .ancestors_difference_stream(ctx, branch_tips.clone(), other_branches)
        .await?
        .map_ok(move |cs| async move { Ok(cs.load(ctx, blobstore).await?) })
        .try_buffered(100)
        .try_collect::<Vec<_>>()
        .await?;

    let bcss: Vec<_> = bcss.into_iter().rev().collect();
    let mut cs_to_parents: HashMap<_, Vec<_>> = HashMap::new();
    for bcs in &bcss {
        let cs_id = bcs.get_changeset_id();
        cs_to_parents.insert(cs_id, bcs.parents().collect());
    }

    // If any of branch_tips hasn't been returned, then it was an ancestor of some of the
    // other_branches.
    for tip in branch_tips {
        if !cs_to_parents.contains_key(&tip) {
            return Ok(None);
        }
    }

    for parents in cs_to_parents.values() {
        for p in parents {
            if !cs_to_parents.contains_key(p) {
                return Ok(None);
            }
        }
    }

    Ok(Some(cs_to_parents.keys().cloned().collect()))
}

async fn delete_bookmark(
    ctx: CoreContext,
    repo: &impl BookmarksRef,
    bookmark: &BookmarkKey,
) -> Result<(), Error> {
    let mut book_txn = repo.bookmarks().create_transaction(ctx.clone());
    let maybe_bookmark_val = repo.bookmarks().get(ctx.clone(), bookmark).await?;
    if let Some(bookmark_value) = maybe_bookmark_val {
        book_txn.delete(bookmark, bookmark_value, BookmarkUpdateReason::XRepoSync)?;
        let res = book_txn.commit().await?;

        if res {
            Ok(())
        } else {
            Err(format_err!("failed to delete a bookmark"))
        }
    } else {
        warn!(
            ctx.logger(),
            "Not deleting '{}' bookmark because it does not exist", bookmark
        );
        Ok(())
    }
}

async fn move_or_create_bookmark(
    ctx: &CoreContext,
    repo: &impl BookmarksRef,
    bookmark: &BookmarkKey,
    cs_id: ChangesetId,
) -> Result<(), Error> {
    let maybe_bookmark_val = repo.bookmarks().get(ctx.clone(), bookmark).await?;

    let mut book_txn = repo.bookmarks().create_transaction(ctx.clone());
    match maybe_bookmark_val {
        Some(old_bookmark_val) => {
            book_txn.update(
                bookmark,
                cs_id,
                old_bookmark_val,
                BookmarkUpdateReason::XRepoSync,
            )?;
        }
        None => {
            book_txn.create(bookmark, cs_id, BookmarkUpdateReason::XRepoSync)?;
        }
    }
    let res = book_txn.commit().await?;

    if res {
        Ok(())
    } else {
        Err(format_err!("failed to move or create a bookmark"))
    }
}

#[cfg(test)]
mod test {
    use bookmarks::BookmarkUpdateLogRef;
    use bookmarks::BookmarksMaybeStaleExt;
    use bookmarks::Freshness;
    use cross_repo_sync::validation;
    use cross_repo_sync_test_utils::init_small_large_repo;
    use cross_repo_sync_test_utils::TestRepo;
    use fbinit::FacebookInit;
    use futures::TryStreamExt;
    use maplit::hashset;
    use mutable_counters::MutableCountersRef;
    use synced_commit_mapping::SqlSyncedCommitMapping;
    use tests_utils::bookmark;
    use tests_utils::resolve_cs_id;
    use tests_utils::CreateCommitContext;
    use tokio::runtime::Runtime;

    use super::*;

    #[fbinit::test]
    fn test_simple(fb: FacebookInit) -> Result<(), Error> {
        let runtime = Runtime::new()?;
        runtime.block_on(async move {
            let ctx = CoreContext::test_mock(fb);
            let (syncers, _, _, _) = init_small_large_repo(&ctx).await?;
            let commit_syncer = syncers.small_to_large;
            let smallrepo = commit_syncer.get_source_repo();

            // Single commit
            let new_master = CreateCommitContext::new(&ctx, &smallrepo, vec!["master"])
                .add_file("newfile", "newcontent")
                .commit()
                .await?;
            bookmark(&ctx, &smallrepo, "master")
                .set_to(new_master)
                .await?;

            sync_and_validate(&ctx, &commit_syncer).await?;

            let non_master_commit = CreateCommitContext::new(&ctx, &smallrepo, vec!["master"])
                .add_file("nonmasterfile", "nonmastercontent")
                .commit()
                .await?;
            bookmark(&ctx, &smallrepo, "nonmasterbookmark")
                .set_to(non_master_commit)
                .await?;

            sync_and_validate(&ctx, &commit_syncer).await?;

            // Create a stack of commits
            let first_in_stack = CreateCommitContext::new(&ctx, &smallrepo, vec!["master"])
                .add_file("stack", "first")
                .commit()
                .await?;

            let second_in_stack = CreateCommitContext::new(&ctx, &smallrepo, vec![first_in_stack])
                .add_file("stack", "second")
                .commit()
                .await?;
            bookmark(&ctx, &smallrepo, "master")
                .set_to(second_in_stack)
                .await?;

            // Create a commit that's based on commit rewritten with noop mapping
            // - it should NOT be rewritten
            let premove = CreateCommitContext::new(&ctx, &smallrepo, vec!["premove"])
                .add_file("premove", "premovecontent")
                .commit()
                .await?;
            bookmark(&ctx, &smallrepo, "newpremove")
                .set_to(premove)
                .await?;

            // Move a bookmark
            bookmark(&ctx, &smallrepo, "newpremove")
                .set_to("premove")
                .await?;
            sync_and_validate(&ctx, &commit_syncer).await?;
            let commit_sync_outcome = commit_syncer
                .get_commit_sync_outcome(&ctx, premove)
                .await?
                .ok_or_else(|| format_err!("commit sync outcome not set"))?;
            match commit_sync_outcome {
                CommitSyncOutcome::RewrittenAs(cs_id, version) => {
                    assert_eq!(version, CommitSyncConfigVersion("noop".to_string()));
                    assert_eq!(cs_id, premove);
                }
                _ => {
                    return Err(format_err!("unexpected outcome"));
                }
            };

            // Delete bookmarks
            bookmark(&ctx, &smallrepo, "newpremove").delete().await?;
            bookmark(&ctx, &smallrepo, "nonmasterbookmark")
                .delete()
                .await?;

            sync_and_validate(&ctx, &commit_syncer).await?;
            Ok(())
        })
    }

    #[fbinit::test]
    fn test_simple_merge(fb: FacebookInit) -> Result<(), Error> {
        let runtime = Runtime::new()?;
        runtime.block_on(async move {
            let ctx = CoreContext::test_mock(fb);
            let (syncers, _, _, _) = init_small_large_repo(&ctx).await?;
            let commit_syncer = syncers.small_to_large;
            let smallrepo = commit_syncer.get_source_repo();

            // Merge new repo
            let first_new_repo = CreateCommitContext::new_root(&ctx, &smallrepo)
                .add_file("firstnewrepo", "newcontent")
                .commit()
                .await?;
            let second_new_repo = CreateCommitContext::new(&ctx, &smallrepo, vec![first_new_repo])
                .add_file("secondnewrepo", "anothercontent")
                .commit()
                .await?;

            bookmark(&ctx, &smallrepo, "newrepohead")
                .set_to(second_new_repo)
                .await?;

            let res = sync(
                &ctx,
                &commit_syncer,
                &hashset! {BookmarkKey::new("master")?},
                PushrebaseRewriteDates::No,
            )
            .await?;
            assert_eq!(res.last(), Some(&SyncResult::SkippedNoKnownVersion));

            let merge = CreateCommitContext::new(&ctx, &smallrepo, vec!["master", "newrepohead"])
                .commit()
                .await?;

            bookmark(&ctx, &smallrepo, "master").set_to(merge).await?;

            sync_and_validate_with_common_bookmarks(
                &ctx,
                &commit_syncer,
                &hashset! {BookmarkKey::new("master")?},
                &hashset! {BookmarkKey::new("newrepohead")?},
                PushrebaseRewriteDates::No,
            )
            .await?;

            // Diamond merges are not allowed
            let diamond_merge =
                CreateCommitContext::new(&ctx, &smallrepo, vec!["master", "newrepohead"])
                    .commit()
                    .await?;
            bookmark(&ctx, &smallrepo, "master")
                .set_to(diamond_merge)
                .await?;
            assert!(sync_and_validate(&ctx, &commit_syncer,).await.is_err());
            Ok(())
        })
    }

    #[fbinit::test]
    fn test_merge_added_in_single_bookmark_update(fb: FacebookInit) -> Result<(), Error> {
        let runtime = Runtime::new()?;
        runtime.block_on(async move {
            let ctx = CoreContext::test_mock(fb);
            let (syncers, _, _, _) = init_small_large_repo(&ctx).await?;
            let commit_syncer = syncers.small_to_large;
            let smallrepo = commit_syncer.get_source_repo();

            // Merge new repo
            let first_new_repo = CreateCommitContext::new_root(&ctx, &smallrepo)
                .add_file("firstnewrepo", "newcontent")
                .commit()
                .await?;
            let second_new_repo = CreateCommitContext::new(&ctx, &smallrepo, vec![first_new_repo])
                .add_file("secondnewrepo", "anothercontent")
                .commit()
                .await?;

            let master_cs_id = resolve_cs_id(&ctx, &smallrepo, "master").await?;
            let merge =
                CreateCommitContext::new(&ctx, &smallrepo, vec![master_cs_id, second_new_repo])
                    .commit()
                    .await?;

            bookmark(&ctx, &smallrepo, "master").set_to(merge).await?;
            sync_and_validate(&ctx, &commit_syncer).await?;

            Ok(())
        })
    }

    #[fbinit::test]
    fn test_merge_of_a_merge_one_step(fb: FacebookInit) -> Result<(), Error> {
        let runtime = Runtime::new()?;
        runtime.block_on(async move {
            let ctx = CoreContext::test_mock(fb);
            let (syncers, _, _, _) = init_small_large_repo(&ctx).await?;
            let commit_syncer = syncers.small_to_large;
            let smallrepo = commit_syncer.get_source_repo();

            // Merge new repo, which itself has a merge
            let first_new_repo = CreateCommitContext::new_root(&ctx, &smallrepo)
                .add_file("firstnewrepo", "newcontent")
                .commit()
                .await?;
            let second_new_repo = CreateCommitContext::new_root(&ctx, &smallrepo)
                .add_file("secondnewrepo", "anothercontent")
                .commit()
                .await?;

            let merge_new_repo =
                CreateCommitContext::new(&ctx, &smallrepo, vec![first_new_repo, second_new_repo])
                    .commit()
                    .await?;

            let master_cs_id = resolve_cs_id(&ctx, &smallrepo, "master").await?;
            let merge =
                CreateCommitContext::new(&ctx, &smallrepo, vec![master_cs_id, merge_new_repo])
                    .commit()
                    .await?;

            bookmark(&ctx, &smallrepo, "master").set_to(merge).await?;
            sync_and_validate(&ctx, &commit_syncer).await?;

            Ok(())
        })
    }

    #[fbinit::test]
    fn test_merge_of_a_merge_two_steps(fb: FacebookInit) -> Result<(), Error> {
        let runtime = Runtime::new()?;
        runtime.block_on(async move {
            let ctx = CoreContext::test_mock(fb);
            let (syncers, _, _, _) = init_small_large_repo(&ctx).await?;
            let commit_syncer = syncers.small_to_large;
            let smallrepo = commit_syncer.get_source_repo();

            // Merge new repo, which itself has a merge
            let first_new_repo = CreateCommitContext::new_root(&ctx, &smallrepo)
                .add_file("firstnewrepo", "newcontent")
                .commit()
                .await?;
            let second_new_repo = CreateCommitContext::new_root(&ctx, &smallrepo)
                .add_file("secondnewrepo", "anothercontent")
                .commit()
                .await?;

            let merge_new_repo =
                CreateCommitContext::new(&ctx, &smallrepo, vec![first_new_repo, second_new_repo])
                    .commit()
                    .await?;
            bookmark(&ctx, &smallrepo, "newrepoimport")
                .set_to(merge_new_repo)
                .await?;
            let res = sync(
                &ctx,
                &commit_syncer,
                &hashset! {BookmarkKey::new("master")?},
                PushrebaseRewriteDates::No,
            )
            .await?;
            assert_eq!(res.last(), Some(&SyncResult::SkippedNoKnownVersion));

            let merge = CreateCommitContext::new(&ctx, &smallrepo, vec!["master", "newrepoimport"])
                .commit()
                .await?;

            bookmark(&ctx, &smallrepo, "master").set_to(merge).await?;
            sync_and_validate_with_common_bookmarks(
                &ctx,
                &commit_syncer,
                &hashset! {BookmarkKey::new("master")?},
                &hashset! {BookmarkKey::new("newrepoimport")?},
                PushrebaseRewriteDates::No,
            )
            .await?;

            Ok(())
        })
    }

    #[fbinit::test]
    fn test_merge_non_shared_bookmark(fb: FacebookInit) -> Result<(), Error> {
        let runtime = Runtime::new()?;
        runtime.block_on(async move {
            let ctx = CoreContext::test_mock(fb);

            let (syncers, _, _, _) = init_small_large_repo(&ctx).await?;
            let commit_syncer = syncers.small_to_large;
            let smallrepo = commit_syncer.get_source_repo();

            let new_repo = CreateCommitContext::new_root(&ctx, &smallrepo)
                .add_file("firstnewrepo", "newcontent")
                .commit()
                .await?;
            bookmark(&ctx, &smallrepo, "newrepohead")
                .set_to(new_repo)
                .await?;
            let res = sync(
                &ctx,
                &commit_syncer,
                &hashset! {BookmarkKey::new("master")?},
                PushrebaseRewriteDates::No,
            )
            .await?;
            assert_eq!(res.last(), Some(&SyncResult::SkippedNoKnownVersion));

            let merge = CreateCommitContext::new(&ctx, &smallrepo, vec!["master", "newrepohead"])
                .commit()
                .await?;

            bookmark(&ctx, &smallrepo, "somebook").set_to(merge).await?;
            assert!(
                sync_and_validate_with_common_bookmarks(
                    &ctx,
                    &commit_syncer,
                    &hashset! {BookmarkKey::new("master")?},
                    &hashset! {BookmarkKey::new("newrepohead")?, BookmarkKey::new("somebook")?},
                    PushrebaseRewriteDates::No,
                )
                .await
                .is_err()
            );
            Ok(())
        })
    }

    #[fbinit::test]
    async fn test_merge_different_versions(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let (syncers, _, _, _) = init_small_large_repo(&ctx).await?;
        let commit_syncer = syncers.small_to_large;
        let smallrepo = commit_syncer.get_source_repo();

        // Merge new repo
        let new_repo = CreateCommitContext::new_root(&ctx, &smallrepo)
            .add_file("firstnewrepo", "newcontent")
            .commit()
            .await?;

        bookmark(&ctx, &smallrepo, "another_pushrebase_bookmark")
            .set_to("premove")
            .await?;
        sync_and_validate_with_common_bookmarks(
            &ctx,
            &commit_syncer,
            &hashset! { BookmarkKey::new("master")?},
            &hashset! {},
            PushrebaseRewriteDates::No,
        )
        .await?;

        let merge = CreateCommitContext::new_root(&ctx, &smallrepo)
            .add_parent("premove")
            .add_parent(new_repo)
            .commit()
            .await?;
        bookmark(&ctx, &smallrepo, "another_pushrebase_bookmark")
            .set_to(merge)
            .await?;

        sync_and_validate_with_common_bookmarks(
             &ctx, &commit_syncer,
             &hashset!{ BookmarkKey::new("master")?, BookmarkKey::new("another_pushrebase_bookmark")?},
             &hashset!{},
                 PushrebaseRewriteDates::No,
         ).await?;

        Ok(())
    }

    async fn sync_and_validate(
        ctx: &CoreContext,
        commit_syncer: &CommitSyncer<SqlSyncedCommitMapping, TestRepo>,
    ) -> Result<(), Error> {
        sync_and_validate_with_common_bookmarks(
            ctx,
            commit_syncer,
            &hashset! {BookmarkKey::new("master")?},
            &hashset! {},
            PushrebaseRewriteDates::No,
        )
        .await
    }

    async fn sync_and_validate_with_common_bookmarks(
        ctx: &CoreContext,
        commit_syncer: &CommitSyncer<SqlSyncedCommitMapping, TestRepo>,
        common_pushrebase_bookmarks: &HashSet<BookmarkKey>,
        should_be_missing: &HashSet<BookmarkKey>,
        pushrebase_rewrite_dates: PushrebaseRewriteDates,
    ) -> Result<(), Error> {
        let smallrepo = commit_syncer.get_source_repo();
        sync(
            ctx,
            commit_syncer,
            common_pushrebase_bookmarks,
            pushrebase_rewrite_dates,
        )
        .await?;

        let actually_missing = validation::find_bookmark_diff(ctx.clone(), commit_syncer)
            .await?
            .into_iter()
            .map(|diff| diff.target_bookmark().clone())
            .collect::<HashSet<_>>();
        println!("actually missing bookmarks: {:?}", actually_missing);
        assert_eq!(&actually_missing, should_be_missing,);

        let heads: Vec<_> = smallrepo
            .bookmarks()
            .get_heads_maybe_stale(ctx.clone())
            .try_collect()
            .await?;
        for head in heads {
            println!("verifying working copy for {}", head);
            validation::verify_working_copy(ctx.clone(), commit_syncer.clone(), head).await?;
        }

        Ok(())
    }

    async fn sync(
        ctx: &CoreContext,
        commit_syncer: &CommitSyncer<SqlSyncedCommitMapping, TestRepo>,
        common_pushrebase_bookmarks: &HashSet<BookmarkKey>,
        pushrebase_rewrite_dates: PushrebaseRewriteDates,
    ) -> Result<Vec<SyncResult>, Error> {
        let smallrepo = commit_syncer.get_source_repo();
        let megarepo = commit_syncer.get_target_repo();

        let counter = crate::format_counter(commit_syncer);
        let start_from = megarepo
            .mutable_counters()
            .get_counter(ctx, &counter)
            .await?
            .unwrap_or(1);

        println!("start from: {}", start_from);
        let read_all = 65536;
        let log_entries: Vec<_> = smallrepo
            .bookmark_update_log()
            .read_next_bookmark_log_entries(
                ctx.clone(),
                start_from as u64,
                read_all,
                Freshness::MostRecent,
            )
            .try_collect()
            .await?;

        println!(
            "syncing log entries {:?}  from repo#{} to repo#{}",
            log_entries,
            smallrepo.repo_identity().id(),
            megarepo.repo_identity().id()
        );

        let mut res = vec![];
        for entry in log_entries {
            let entry_id = entry.id;
            let single_res = sync_single_bookmark_update_log(
                ctx,
                commit_syncer,
                entry,
                common_pushrebase_bookmarks,
                MononokeScubaSampleBuilder::with_discard(),
                pushrebase_rewrite_dates,
            )
            .await?;
            res.push(single_res);

            megarepo
                .mutable_counters()
                .set_counter(ctx, &counter, entry_id, None)
                .await?;
        }

        Ok(res)
    }
}
