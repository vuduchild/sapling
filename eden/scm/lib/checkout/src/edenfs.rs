/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
#[cfg(unix)]
use std::os::unix::prelude::MetadataExt;
use std::process::Command;
use std::sync::Arc;

use anyhow::Result;
use async_runtime::try_block_unless_interrupted as block_on;
use configmodel::Config;
use configmodel::ConfigExt;
use edenfs_client::CheckoutConflict;
use io::IO;
use pathmatcher::AlwaysMatcher;
use repo::repo::Repo;
use spawn_ext::CommandExt;
use treestate::filestate::StateFlags;
use types::HgId;
use types::RepoPath;
use workingcopy::util::walk_treestate;
use workingcopy::workingcopy::WorkingCopy;

use crate::errors::EdenConflictError;

pub fn edenfs_checkout(
    io: &IO,
    repo: &mut Repo,
    wc: &mut WorkingCopy,
    target_commit: HgId,
    checkout_mode: edenfs_client::CheckoutMode,
) -> anyhow::Result<()> {
    // For now this just supports Force
    assert_eq!(checkout_mode, edenfs_client::CheckoutMode::Force);
    let target_commit_tree_hash = block_on(repo.get_root_tree_id(target_commit.clone()))?;
    let conflicts =
        wc.eden_client()?
            .checkout(target_commit, target_commit_tree_hash, checkout_mode)?;
    abort_on_eden_conflict_error(repo.config(), conflicts)?;
    let mergepath = wc.dot_hg_path().join("merge");
    remove_dir_all(mergepath.as_path()).ok();
    clear_edenfs_dirstate(wc)?;
    wc.set_parents(vec![target_commit], Some(target_commit_tree_hash))?;
    wc.treestate().lock().flush()?;
    let updatestate_path = wc.dot_hg_path().join("updatestate");
    util::file::unlink_if_exists(updatestate_path)?;
    edenfs_redirect_fixup(io, repo.config(), wc)?;
    Ok(())
}

fn clear_edenfs_dirstate(wc: &mut WorkingCopy) -> anyhow::Result<()> {
    let tbind = wc.treestate();
    let mut treestate = tbind.lock();
    let matcher = Arc::new(AlwaysMatcher::new());
    let mask = StateFlags::EXIST_P1 | StateFlags::EXIST_P2 | StateFlags::EXIST_NEXT;
    let mut tracked = Vec::new();
    walk_treestate(
        &mut treestate,
        matcher,
        StateFlags::empty(),
        mask,
        StateFlags::empty(),
        |path, _state| {
            tracked.push(path);
            Ok(())
        },
    )?;
    for path in tracked {
        treestate.remove(path.as_byte_slice())?;
    }
    Ok(())
}

/// run `edenfsctl redirect fixup`, potentially in background.
///
/// If the `.eden-redirections` file does not exist in the working copy,
/// or is empty, run nothing.
///
/// Otherwise, parse the fixup directories, if they exist and look okay,
/// run `edenfsctl redirect fixup` in background. This reduces overhead
/// especially on Windows.
///
/// Otherwise, run in foreground. This is needed for automation that relies
/// on `checkout HASH` to setup critical repo redirections.
pub fn edenfs_redirect_fixup(io: &IO, config: &dyn Config, wc: &WorkingCopy) -> anyhow::Result<()> {
    let is_okay = match is_edenfs_redirect_okay(wc).unwrap_or(Some(false)) {
        Some(r) => r,
        None => return Ok(()),
    };
    let arg0 = config.get_or("edenfs", "command", || "edenfsctl".to_owned())?;
    let args_raw = config.get_or("edenfs", "redirect-fixup", || "redirect fixup".to_owned())?;
    let args = args_raw.split_whitespace().collect::<Vec<_>>();
    let mut cmd0 = Command::new(arg0);
    let cmd = cmd0.args(args);
    if is_okay {
        cmd.spawn_detached()?;
    } else {
        io.disable_progress(true)?;
        let status = cmd.status();
        io.disable_progress(false)?;
        status?;
    }
    Ok(())
}

/// Whether the edenfs redirect directories look okay, or None if redirect is unnecessary.
fn is_edenfs_redirect_okay(wc: &WorkingCopy) -> anyhow::Result<Option<bool>> {
    let vfs = wc.vfs();
    let mut redirections = HashMap::new();

    // Check edenfs-client/src/redirect.rs for the config paths and file format.
    let client_paths = vec![
        wc.vfs().root().join(".eden-redirections"),
        wc.eden_client()?.client_path().join("config.toml"),
    ];

    for path in client_paths {
        // Cannot use vfs::read as config.toml is outside of the working copy
        let text = match read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::debug!("is_edenfs_redirect_okay failed to check: {}", e);
                return Ok(Some(false));
            }
        };
        if let Ok(s) = toml::from_str::<toml::Table>(text.as_str()) {
            if let Some(r) = s.get("redirections").and_then(|v| v.as_table()) {
                for (k, v) in r.iter() {
                    redirections.insert(k.to_owned(), v.to_string());
                }
            }
        }
    }

    if redirections.is_empty() {
        return Ok(None);
    }

    #[cfg(unix)]
    let root_device_inode = vfs.metadata(RepoPath::empty())?.dev();
    for (path, kind) in redirections.into_iter() {
        let path_metadata = match vfs.metadata(RepoPath::from_str(path.as_str())?) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if cfg!(windows) || kind == "symlink" {
            // kind is "bind" or "symlink". On Windows, "bind" is not supported
            if !path_metadata.is_symlink() {
                return Ok(Some(false));
            }
        } else {
            #[cfg(unix)]
            // Bind mount should have a different device inode
            if path_metadata.dev() == root_device_inode {
                return Ok(Some(false));
            }
        }
    }

    Ok(Some(true))
}

/// abort if there is a ConflictType.ERROR type of conflicts
pub fn abort_on_eden_conflict_error(
    config: &dyn Config,
    conflicts: Vec<CheckoutConflict>,
) -> Result<(), EdenConflictError> {
    if !config
        .get_or_default::<bool>("experimental", "abort-on-eden-conflict-error")
        .unwrap_or_default()
    {
        return Ok(());
    }
    for conflict in conflicts {
        if edenfs_client::ConflictType::Error == conflict.conflict_type {
            hg_metrics::increment_counter("abort_on_eden_conflict_error", 1);
            return Err(EdenConflictError {
                path: conflict.path.into_string(),
                message: conflict.message,
            });
        }
    }
    Ok(())
}
