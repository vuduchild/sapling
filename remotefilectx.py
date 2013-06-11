# remoterevlog.py - revlog implementation where the content is stored remotely
#
# Copyright 2013 Facebook, Inc.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

import fileserverclient
import collections, os
from mercurial.node import bin, hex, nullid, nullrev, short
from mercurial import revlog, mdiff, filelog, context, util, error

propertycache = util.propertycache

class remotefilectx(context.filectx):
    def __init__(self, repo, path, changeid=None, fileid=None,
                 filelog=None, changectx=None, ancestormap=None):
        if fileid == nullrev:
            fileid = nullid
        if fileid and len(fileid) == 40:
            fileid = bin(fileid)
        super(remotefilectx, self).__init__(repo, path, changeid,
            fileid, filelog, changectx)
        self._ancestormap = ancestormap

    def __str__(self):
        return "%s@%s" % (self.path(), short(self.node()))

    def size(self):
        return self._filelog.size(self._filenode)

    @propertycache
    def _changeid(self):
        if '_changeid' in self.__dict__:
            return self._changeid
        elif '_changectx' in self.__dict__:
            return self._changectx.rev()
        else:
            return self.linkrev()

    def filectx(self, fileid):
        '''opens an arbitrary revision of the file without
        opening a new filelog'''
        return remotefilectx(self._repo, self._path, fileid=fileid,
                             filelog=self._filelog)

    def linkrev(self):
        if self._fileid == nullid:
            return nullrev

        ancestormap = self.ancestormap()
        p1, p2, linknode, copyfrom = ancestormap[self._fileid]
        return self._repo.changelog.rev(linknode)

    def renamed(self):
        """check if file was actually renamed in this changeset revision

        If rename logged in file revision, we report copy for changeset only
        if file revisions linkrev points back to the changeset in question
        or both changeset parents contain different file revisions.
        """
        ancestormap = self.ancestormap()

        p1, p2, linknode, copyfrom = ancestormap[self._filenode]
        if not copyfrom:
            return None

        renamed = (copyfrom, p1)
        if self.rev() == self.linkrev():
            return renamed

        name = self.path()
        fnode = self._filenode
        for p in self._changectx.parents():
            try:
                if fnode == p.filenode(name):
                    return None
            except error.LookupError:
                pass
        return renamed

    def ancestormap(self):
        if not self._ancestormap:
            self._ancestormap = self.filelog().ancestormap(self._filenode)

        return self._ancestormap

    def parents(self):
        repo = self._repo
        ancestormap = self.ancestormap()

        p1, p2, linknode, copyfrom = ancestormap[self._filenode]
        results = []
        if p1 != nullid:
            path = copyfrom or self._path
            flog = repo.file(path)
            p1ctx = remotefilectx(repo, path, fileid=p1, filelog=flog,
                                  ancestormap=ancestormap)
            results.append(p1ctx)

        if p2 != nullid:
            path = self._path
            flog = repo.file(path)
            p2ctx = remotefilectx(repo, path, fileid=p2, filelog=flog,
                                  ancestormap=ancestormap)
            results.append(p2ctx)

        return results

    def ancestors(self, followfirst=False):
        repo = self._repo
        ancestormap = self.ancestormap()

        queue = [(self.path(), self.filenode())]
        while queue:
            path, node = queue.pop(0)
            p1, p2, linknode, copyfrom = ancestormap[node]
            if p1 != nullid:
                p1path = copyfrom or path
                flog = repo.file(p1path)
                yield remotefilectx(repo, p1path, fileid=p1, filelog=flog,
                                    ancestormap=ancestormap)
                queue.append((p1path, p1))

            if p2 != nullid and not followfirst:
                flog = repo.file(path)
                yield remotefilectx(repo, path, fileid=p2, filelog=flog,
                                    ancestormap=ancestormap)
                queue.append((path, p2))

    def annotate(self, follow=False, linenumber=None, diffopts=None):
        # use linkrev to find the first changeset where self appeared
        if self.rev() != self.linkrev():
            base = self.filectx(self.filenode())
        else:
            base = self

        fetch = []
        ancestors = base.ancestors()
        for ancestor in ancestors:
            fetch.append((ancestor.path(), hex(ancestor.filenode())))

        storepath = self._repo.sopener.vfs.base
        fileserverclient.client.prefetch(storepath, fetch)

        return super(remotefilectx, self).annotate(follow, linenumber, diffopts)

    def cmp(self, fctx):
        """compare with other file context

        returns True if different than fctx.
        """
        if (self.size() == fctx.size() or
            self._repo._encodefilterpats):
            return self._filelog.cmp(self._filenode, fctx.data())

        return True

class remoteworkingfilectx(context.workingfilectx, remotefilectx):
    def __init__(self, repo, path, filelog=None, workingctx=None):
        self._ancestormap = None
        return super(remoteworkingfilectx, self).__init__(repo, path, 
            filelog, workingctx)

    def parents(self):
        return remotefilectx.parents(self)

    def ancestormap(self):
        if not self._ancestormap:
            path = self._path
            pcl = self._changectx._parents
            renamed = self.renamed()

            if renamed:
                p1 = renamed
            else:
                p1 = (path, pcl[0]._manifest.get(path, nullid))

            p2 = (path, nullid)
            if len(pcl) > 1:
                p2 = (path, pcl[1]._manifest.get(path, nullid))

            m = {}
            if p1[1] != nullid:
                p1ctx = self._repo.filectx(p1[0], fileid=p1[1])
                m.update(p1ctx.filelog().ancestormap(p1[1]))

            if p2[1] != nullid:
                p2ctx = self._repo.filectx(p2[0], fileid=p2[1])
                m.update(p2ctx.filelog().ancestormap(p2[1]))

            copyfrom = ''
            if renamed:
                copyfrom = renamed[0]
            m[None] = (p1[1], p2[1], nullid, copyfrom)
            self._ancestormap = m

        return self._ancestormap
