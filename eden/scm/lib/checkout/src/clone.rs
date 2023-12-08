/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use configmodel::convert::ByteCount;
use configmodel::Config;
use configmodel::ConfigExt;
use manifest_tree::Diff;
use manifest_tree::TreeManifest;
use pathmatcher::Matcher;
use progress_model::ProgressBar;
use repolock::RepoLocker;
use storemodel::FileStore;
use tracing::instrument;
use treestate::dirstate::Dirstate;
use treestate::dirstate::TreeStateFields;
use treestate::serialization::Serializable;
use treestate::treestate::TreeState;
use types::hgid::NULL_ID;
use types::HgId;
use util::file::atomic_open;
use util::file::atomic_write;
use util::path::remove_file;
use vfs::VFS;
use workingcopy::sparse;

use crate::errors::CheckoutError;
use crate::file_state;
use crate::ActionMap;
use crate::Checkout;
use crate::CheckoutPlan;

pub struct CheckoutStats {
    pub updated: usize,
    pub merged: usize,
    pub removed: usize,
    pub unresolved: usize,
}

impl std::fmt::Display for CheckoutStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for (name, val) in [
            ("updated", self.updated),
            ("merged", self.merged),
            ("removed", self.removed),
            ("unresolved", self.unresolved),
        ] {
            if val == 0 {
                continue;
            }

            if !first {
                write!(f, ", ")?;
            }
            first = false;

            write!(f, "{} files {}", val, name)?;
        }

        Ok(())
    }
}
/// A somewhat simplified/specialized checkout suitable for use during a clone.
#[instrument(skip_all, fields(path=%dot_path.display(), %target), err)]
pub fn checkout(
    config: &dyn Config,
    dot_path: &Path,
    source_mf: &TreeManifest,
    target_mf: &TreeManifest,
    file_store: Arc<dyn FileStore>,
    ts: &mut TreeState,
    target: HgId,
    locker: &RepoLocker,
) -> anyhow::Result<CheckoutStats, CheckoutError> {
    let mut state = CheckoutState::default();
    state
        .checkout(
            config, dot_path, source_mf, target_mf, file_store, ts, target, locker,
        )
        .map_err(|err| CheckoutError {
            resumable: state.resumable,
            source: err,
        })
}

#[derive(Default)]
struct CheckoutState {
    resumable: bool,
}

impl CheckoutState {
    fn checkout(
        &mut self,
        config: &dyn Config,
        dot_path: &Path,
        source_mf: &TreeManifest,
        target_mf: &TreeManifest,
        file_store: Arc<dyn FileStore>,
        ts: &mut TreeState,
        target: HgId,
        locker: &RepoLocker,
    ) -> anyhow::Result<CheckoutStats> {
        let wc_path = match dot_path.parent() {
            Some(p) => p,
            None => bail!("invalid dot path {}", dot_path.display()),
        };

        let _wlock = locker.lock_working_copy(dot_path.to_owned())?;

        let mut sparse_overrides = None;

        let vfs = VFS::new(wc_path.to_path_buf())?;

        let (matcher, _hash): (Arc<dyn Matcher + Sync + Send>, u64) =
            match util::file::exists(dot_path.join("sparse"))? {
                Some(_) => {
                    let overrides = sparse::config_overrides(config);
                    sparse_overrides = Some(overrides.clone());
                    sparse::repo_matcher_with_overrides(
                        &vfs,
                        dot_path,
                        target_mf.clone(),
                        file_store.clone(),
                        &overrides,
                    )?
                }
                None => None,
            }
            .unwrap_or_else(|| (Arc::new(pathmatcher::AlwaysMatcher::new()), 0));

        let diff =
            Diff::new(source_mf, target_mf, &matcher).context("error creating checkout diff")?;
        let actions = ActionMap::from_diff(diff).context("error creating checkout action map")?;

        let checkout = Checkout::from_config(vfs.clone(), config)?;
        let mut plan = checkout.plan_action_map(actions);

        // Write out overrides first so they don't change when resuming
        // this checkout.
        if let Some(sparse_overrides) = sparse_overrides {
            atomic_write(&dot_path.join(sparse::CONFIG_OVERRIDE_CACHE), |f| {
                serde_json::to_writer(f, &sparse_overrides)?;
                Ok(())
            })?;
        }

        if config.get_or_default("checkout", "resumable")? {
            let progress_path = dot_path.join("updateprogress");
            plan.add_progress(&progress_path).with_context(|| {
                format!(
                    "error loading checkout progress '{}'",
                    progress_path.display()
                )
            })?;
            self.resumable = true;
        }

        atomic_write(&dot_path.join("updatestate"), |f| {
            f.write_all(target.to_hex().as_bytes())
        })?;

        plan.apply_store(file_store.as_ref())?;

        ts.set_metadata(BTreeMap::from([("p1".to_string(), target.to_hex())]))?;

        update_dirstate(&plan, ts, &vfs)?;
        flush_dirstate(config, ts, dot_path, target)?;

        remove_file(dot_path.join("updatestate"))?;

        Ok(CheckoutStats {
            updated: plan.stats().0,
            merged: 0,
            removed: 0,
            unresolved: 0,
        })
    }
}

#[instrument(skip_all, err)]
fn update_dirstate(plan: &CheckoutPlan, ts: &mut TreeState, vfs: &VFS) -> anyhow::Result<()> {
    let (update_count, remove_count) = plan.stats();
    let bar = ProgressBar::new_adhoc("recording", (update_count + remove_count) as u64, "files");

    // Probably not required for clone.
    for removed in plan.removed_files() {
        ts.remove(removed)?;
        bar.increase_position(1);
    }

    for updated in plan
        .updated_content_files()
        .chain(plan.updated_meta_files())
    {
        let fstate = file_state(vfs, updated)?;
        ts.insert(updated, &fstate)?;
        bar.increase_position(1);
    }

    Ok(())
}

pub fn flush_dirstate(
    config: &dyn Config,
    ts: &mut TreeState,
    dot_hg_path: &Path,
    target: HgId,
) -> anyhow::Result<()> {
    // Flush treestate then write .hg/dirstate that points to the
    // current treestate file.

    let dirstate_path = dot_hg_path.join("dirstate");
    let mut dirstate_file = atomic_open(&dirstate_path)?;

    // Get "now" from the atomic temp file we just created's mtime.
    // This ensures we use a sane mtime in case the file system
    // doesn't match our local clock, for whatever reason.
    let now = dirstate_file
        .as_file()
        .metadata()?
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();

    // Invalidate entries with mtime >= now so we can notice user
    // edits to files in the same second the checkout completes.
    ts.invalidate_mtime(now.try_into()?)
        .context("error invalidating dirstate mtime")?;

    let tree_root_id = ts.flush()?;

    let tree_file = ts
        .path()
        .ok_or_else(|| anyhow!("TreeState shouldn't be backed by memory but a physical file"))?
        .file_name()
        .ok_or_else(|| anyhow!("bad treestate path: {:?}", ts.path()))?;

    let mut threshold = 0;
    let min_repack_threshold = config
        .get_or_default::<ByteCount>("treestate", "minrepackthreshold")?
        .value();
    if tree_root_id.0 > min_repack_threshold {
        if let Some(factor) = config.get_nonempty_opt::<u64>("treestate", "repackfactor")? {
            threshold = tree_root_id.0 * factor;
        }
    }
    let ds = Dirstate {
        p1: target,
        p2: NULL_ID,
        tree_state: Some(TreeStateFields {
            tree_filename: tree_file.to_owned().into_string().map_err(|_| {
                anyhow!(
                    "can't convert treestate file name to String: {:?}",
                    tree_file
                )
            })?,
            tree_root_id,
            repack_threshold: Some(threshold),
        }),
    };

    ds.serialize(dirstate_file.as_file())?;

    dirstate_file.save()?;

    Ok(())
}
