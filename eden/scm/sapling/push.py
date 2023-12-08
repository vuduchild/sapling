# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from collections import defaultdict
from typing import List, Optional, Tuple

from . import edenapi_upload, error, hg, mutation, phases, scmutil
from .bookmarks import readremotenames, saveremotenames
from .i18n import _
from .node import bin, hex, nullhex, short


def get_edenapi_for_dest(repo, _dest):
    """Get an EdenApi instance for the given destination."""
    if not repo.ui.configbool("push", "edenapi"):
        return None

    # We are focusing the prod case for now, which means we assume the
    # default push dest is the same as edenapi.url config.
    try:
        edenapi = repo.edenapi
        if edenapi.url().startswith("eager:"):
            # todo (zhaolong): implement push related EdenAPIs for eagerepo
            return None

        return edenapi
    except Exception:
        return None


def push(repo, dest, head_node, remote_bookmark, opargs=None):
    """Push via EdenApi (HTTP)"""
    ui = repo.ui
    edenapi = get_edenapi_for_dest(repo, dest)
    opargs = opargs or {}

    ui.status_err(
        _("pushing rev %s to destination %s bookmark %s\n")
        % (short(head_node), edenapi.url(), remote_bookmark)
    )

    # upload revs via EdenApi
    uploaded, failed = edenapi_upload.uploadhgchangesets(repo, [head_node])
    if failed:
        raise error.Abort(
            _("failed to upload commits to server: {}").format(
                [repo[node].hex() for node in failed]
            )
        )
    ui.debug(f"uploaded {len(uploaded)} new commits\n")

    bookmark_node = get_remote_bookmark_node(ui, edenapi, remote_bookmark)

    # create remote bookmark
    if bookmark_node is None:
        if opargs.get("create"):
            create_remote_bookmark(ui, edenapi, remote_bookmark, head_node, opargs)
            ui.debug("remote bookmark %s created\n" % remote_bookmark)
            record_remote_bookmark(repo, remote_bookmark, head_node)
            return 0
        else:
            raise error.Abort(
                _("could not find remote bookmark '%s', use '--create' to create it")
                % remote_bookmark
            )

    if repo[head_node].phase() == phases.public:
        # if the head is already a public commit, then do a plain push (no pushrebase)
        plain_push(repo, edenapi, remote_bookmark, head_node, bookmark_node, opargs)
    else:
        # update the exiting bookmark with push rebase
        return push_rebase(repo, dest, head_node, remote_bookmark, opargs)


def plain_push(repo, edenapi, bookmark, to_node, from_node, opargs=None):
    """Plain push without rebasing."""
    pushvars = parse_pushvars(opargs.get("pushvars"))

    # setbookmark api server logic does not check if it's a non fast-forward move,
    # let's check it in the client side as a workaround for now
    is_ancestor = repo.dageval(lambda: isancestor(from_node, to_node))
    if not is_ancestor:
        if not is_true(pushvars.get("NON_FAST_FORWARD")):
            raise error.Abort(
                _(
                    "non-fast-forward push to remote bookmark %s from %s to %s "
                    "(set pushvar NON_FAST_FORWARD=true for a non-fast-forward move)"
                )
                % (bookmark, short(from_node), short(to_node)),
            )

    repo.ui.status(
        _("moving remote bookmark %s from %s to %s\n")
        % (bookmark, short(from_node), short(to_node))
    )
    result = edenapi.setbookmark(bookmark, to_node, from_node, pushvars)["data"]
    if "Err" in result:
        raise error.Abort(_("server error: %s") % result["Err"]["message"])

    record_remote_bookmark(repo, bookmark, to_node)


def push_rebase(repo, dest, head_node, remote_bookmark, opargs=None):
    """Update the remote bookmark with server side rebase.

    For updating the existing remote bookmark, push_rebase allows the server to
    rebase incoming commits as part of the push process. This helps solve the
    problem of push contention where many clients try to push at once and
    all but one fail. Instead of failing, it will rebase the incoming commit
    onto the target bookmark (i.e. @ or master) as long as the commit doesn't touch
    any files that have been modified in the target bookmark. Put another way,
    push_rebase will not perform any file content merges. It only performs the
    rebase when there is no chance of a file merge.
    """
    ui, edenapi = repo.ui, repo.edenapi
    bookmark = remote_bookmark
    wnode = repo["."].node()
    ui.write(_("updating remote bookmark %s\n") % bookmark)

    # according to the Mononoke API (D23813368), base is the parent of the bottom of the stack
    # that is to be landed.
    draft_nodes = repo.dageval(lambda: roots(ancestors([head_node]) & draft()))
    if len(draft_nodes) > 1:
        # todo (zhaolong): handle merge commit
        raise error.Abort(_("multiple roots found for stack %s") % short(head_node))

    parents = repo[draft_nodes[0]].parents()
    if len(parents) != 1:
        raise error.Abort(
            _("%d parents found for commit %s") % (len(parents), short(draft_nodes[0]))
        )
    base = parents[0].node()

    pushvars = parse_pushvars(opargs.get("pushvars"))
    response = edenapi.landstack(bookmark, head=head_node, base=base, pushvars=pushvars)

    result = response["data"]
    if "Err" in result:
        raise error.Abort(_("Server error: %s") % result["Err"]["message"])

    data = result["Ok"]
    new_head = data["new_head"]
    old_to_new_hgids = data["old_to_new_hgids"]

    with repo.wlock(), repo.lock(), repo.transaction("pushrebase"):
        repo.pull(
            source=dest,
            bookmarknames=(bookmark,),
            remotebookmarks={bookmark: new_head},
        )

        if wnode in old_to_new_hgids:
            ui.note(_("moving working copy parent\n"))
            hg.update(repo, old_to_new_hgids[wnode])

        replacements = {old: [new] for old, new in old_to_new_hgids.items()}
        scmutil.cleanupnodes(repo, replacements, "pushrebase")

        entries = [
            mutation.createsyntheticentry(repo, [node], new_node, "pushrebase")
            for (node, new_node) in old_to_new_hgids.items()
        ]
        mutation.recordentries(repo, entries, skipexisting=False)

        ui.write(_("updated remote bookmark %s to %s\n") % (bookmark, short(new_head)))
        return 0


def get_remote_bookmark_node(ui, edenapi, bookmark) -> Optional[bytes]:
    ui.debug("getting remote bookmark %s\n" % bookmark)
    response = edenapi.bookmarks([bookmark])
    hexnode = response.get(bookmark)
    return bin(hexnode) if hexnode else None


def create_remote_bookmark(ui, edenapi, bookmark, node, opargs) -> None:
    ui.write(_("creating remote bookmark %s\n") % bookmark)
    pushvars = parse_pushvars(opargs.get("pushvars"))
    result = edenapi.setbookmark(bookmark, node, None, pushvars=pushvars)["data"]
    if "Err" in result:
        raise error.Abort(
            _("failed to create remote bookmark:\n  remote server error: %s")
            % result["Err"]["message"]
        )


def record_remote_bookmark(repo, bookmark, new_node) -> None:
    """Record a remote bookmark in vfs.

    * bookmark - the name of the remote bookmark to update, e.g. "main"
    """
    with repo.wlock(), repo.lock(), repo.transaction("recordremotebookmark"):
        data = defaultdict(dict)  # {'remote': {'master': '<commit hash>'}}
        for hexnode, _nametype, remote, name in readremotenames(repo):
            data[remote][name] = hexnode
        remote = repo.ui.config("remotenames", "hoist")
        data.setdefault(remote, {})[bookmark] = hex(new_node)
        saveremotenames(repo, data)


def delete_remote_bookmark(repo, edenapi, bookmark, pushvars_strs) -> None:
    ui = repo.ui
    node = get_remote_bookmark_node(ui, edenapi, bookmark)
    if node is None:
        raise error.Abort(_("remote bookmark %s does not exist") % bookmark)

    # delete remote bookmark from server
    ui.write(_("deleting remote bookmark %s\n") % bookmark)
    pushvars = parse_pushvars(pushvars_strs)
    result = edenapi.setbookmark(bookmark, None, node, pushvars=pushvars)["data"]
    if "Err" in result:
        raise error.Abort(
            _("failed to delete remote bookmark:\n  remote server error: %s")
            % result["Err"]["message"]
        )

    # delete remote bookmark from repo
    remote = repo.ui.config("remotenames", "hoist")
    remotenamechanges = {bookmark: nullhex}
    saveremotenames(repo, {remote: remotenamechanges}, override=False)


### utils


def parse_pushvars(pushvars_strs: Optional[List[str]]) -> List[Tuple[str, str]]:
    kvs = pushvars_strs or []
    pushvars = {}
    for kv in kvs:
        try:
            k, v = kv.split("=", 1)
        except ValueError:
            raise error.Abort(
                _("invalid pushvar: '%s', expecting 'key=value' format") % kv
            )
        pushvars[k] = v
    return pushvars


def is_true(s: Optional[str]) -> bool:
    return s == "true" or s == "True"
