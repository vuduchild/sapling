# Portions Copyright (c) Facebook, Inc. and its affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# commandserver.py - communicate with Mercurial's API over a pipe
#
#  Copyright Matt Mackall <mpm@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

from __future__ import absolute_import

import errno
import gc
import os
import random
import signal
import socket
import struct
import sys
import traceback
from typing import Any, BinaryIO, List, Tuple

from . import encoding, error, pycompat, util
from .i18n import _


if sys.version_info.major == 3:
    import selectors as selectors2
else:
    from .thirdparty import selectors2


logfile = None


def log(*args):
    if not logfile:
        return

    for a in args:
        logfile.write(str(a))

    logfile.flush()


class channeledoutput(object):
    """
    Write data to out in the following format:

    data length (unsigned int),
    data
    """

    def __init__(self, out, channel):
        # type: (BinaryIO, bytes) -> None
        assert isinstance(channel, bytes)
        assert len(channel) == 1
        self.out = out
        self.channel = channel

    @property
    def name(self):
        # type: () -> str
        return "<%c-channel>" % self.channel

    def write(self, data):
        # type: (bytes) -> None
        if not data:
            return
        # single write() to guarantee the same atomicity as the underlying file
        self.out.write(struct.pack(">cI", self.channel, len(data)) + data)
        self.out.flush()

    def __getattr__(self, attr):
        if attr in ("isatty", "fileno", "tell", "seek"):
            raise AttributeError(attr)
        return getattr(self.out, attr)


class channeledinput(object):
    """
    Read data from in_.

    Requests for input are written to out in the following format:
    channel identifier - 'I' for plain input, 'L' line based (1 byte)
    how many bytes to send at most (unsigned int),

    The client replies with:
    data length (unsigned int), 0 meaning EOF
    data
    """

    maxchunksize = 4 * 1024

    def __init__(self, in_, out, channel):
        # type: (BinaryIO, BinaryIO, bytes) -> None
        self.in_ = in_
        self.out = out
        self.channel = channel

    @property
    def name(self):
        # type: () -> str
        return "<%c-channel>" % pycompat.decodeutf8(self.channel)

    def read(self, size=-1):
        # type: (int) -> bytes
        if size < 0:
            # if we need to consume all the clients input, ask for 4k chunks
            # so the pipe doesn't fill up risking a deadlock
            size = self.maxchunksize
            s = self._read(size, self.channel)
            buf = s
            while s:
                s = self._read(size, self.channel)
                buf += s

            return buf
        else:
            return self._read(size, self.channel)

    def _read(self, size, channel):
        # type: (int, bytes) -> bytes
        if not size:
            return b""
        assert size > 0

        # tell the client we need at most size bytes
        self.out.write(struct.pack(">cI", channel, size))
        self.out.flush()

        length = self.in_.read(4)
        length = struct.unpack(">I", length)[0]
        if not length:
            return b""
        else:
            return self.in_.read(length)

    def readline(self, size=-1):
        # type: (int) -> bytes
        if size < 0:
            size = self.maxchunksize
            s = self._read(size, b"L")
            buf = s
            # keep asking for more until there's either no more or
            # we got a full line
            while s and s[-1] != b"\n":
                s = self._read(size, b"L")
                buf += s

            return buf
        else:
            return self._read(size, b"L")

    def __iter__(self):
        # type: () -> channeledinput
        return self

    def next(self):
        # type: () -> bytes
        l = self.readline()
        if not l:
            raise StopIteration
        return l

    def __getattr__(self, attr):
        if attr in ("isatty", "fileno", "tell", "seek"):
            raise AttributeError(attr)
        return getattr(self.in_, attr)


class server(object):
    """
    Listens for commands on fin, runs them and writes the output on a channel
    based stream to fout.
    """

    def __init__(self, ui, repo, fin, fout):
        # type: (Any, Any, BinaryIO, BinaryIO) -> None
        self.cwd = pycompat.getcwd()

        # developer config: cmdserver.log
        logpath = ui.config("cmdserver", "log")
        if logpath:
            global logfile
            if logpath == "-":
                # write log on a special 'd' (debug) channel
                # pyre-fixme[9]: logfile has type `None`; used as `channeledoutput`.
                logfile = channeledoutput(fout, b"d")
            else:
                # pyre-fixme[9]: logfile has type `None`; used as `TextIOWrapper`.
                logfile = open(logpath, "a")

        if repo:
            # the ui here is really the repo ui so take its baseui so we don't
            # end up with its local configuration
            self.ui = repo.baseui
            self.repo = repo
            self.repoui = repo.ui
        else:
            self.ui = ui
            self.repo = self.repoui = None

        self.cerr = channeledoutput(fout, b"e")
        self.cout = channeledoutput(fout, b"o")
        self.cin = channeledinput(fin, fout, b"I")
        self.cresult = channeledoutput(fout, b"r")

        self.client = fin

    def cleanup(self):
        """release and restore resources taken during server session"""

    def _read(self, size):
        # type: (int) -> bytes
        if not size:
            return b""

        data = self.client.read(size)

        # is the other end closed?
        if not data:
            raise EOFError

        return data

    def _readstr(self):
        # type: () -> bytes
        """read a string from the channel

        format:
        data length (uint32), data
        """
        length = struct.unpack(">I", self._read(4))[0]
        if not length:
            return b""
        return self._read(length)

    def _readlist(self):
        # type: () -> List[str]
        """read a list of NULL separated strings from the channel"""
        s = self._readstr()
        if s:
            s = pycompat.decodeutf8(s)
            return s.split("\0")
        else:
            return []

    def runcommand(self):
        """ reads a list of \0 terminated arguments, executes
        and writes the return code to the result channel """
        from . import dispatch  # avoid cycle

        args = self._readlist()
        pycompat.sysargv[1:] = args

        # copy the uis so changes (e.g. --config or --verbose) don't
        # persist between requests
        copiedui = self.ui.copy()
        uis = [copiedui]
        if self.repo:
            self.repo.baseui = copiedui
            # clone ui without using ui.copy because this is protected
            repoui = self.repoui.__class__(self.repoui)
            repoui.copy = copiedui.copy  # type: ignore , redo copy protection
            uis.append(repoui)
            self.repo.ui = self.repo.dirstate._ui = repoui
            self.repo.invalidateall()

        for ui in uis:
            ui.resetstate()
            # any kind of interaction must use server channels, but chg may
            # replace channels by fully functional tty files. so nontty is
            # enforced only if cin is a channel.
            if not util.safehasattr(self.cin, "fileno"):
                ui.setconfig("ui", "nontty", "true", "commandserver")

        req = dispatch.request(
            args[:], copiedui, self.repo, self.cin, self.cout, self.cerr
        )

        ret = (dispatch.dispatch(req) or 0) & 255  # might return None

        # restore old cwd
        if "--cwd" in args:
            os.chdir(self.cwd)

        self.cresult.write(struct.pack(">i", int(ret)))

    def getencoding(self):
        # type: () -> None
        """ writes the current encoding to the result channel """
        self.cresult.write(pycompat.encodeutf8(encoding.encoding))

    def serveone(self):
        # type: () -> bool
        cmd = self.client.readline()[:-1]
        cmd = pycompat.decodeutf8(cmd)
        if cmd:
            handler = self.capabilities.get(cmd)
            if handler:
                handler(self)
            else:
                # clients are expected to check what commands are supported by
                # looking at the servers capabilities
                raise error.Abort(_("unknown command %s") % cmd)

        return cmd != ""

    capabilities = {"runcommand": runcommand, "getencoding": getencoding}

    def serve(self):
        # type: () -> int
        hellomsg = "capabilities: " + " ".join(sorted(self.capabilities))
        hellomsg += "\n"
        # pyre-fixme[6]: `+` is not supported for operand types `str` and `None`.
        hellomsg += "encoding: " + encoding.encoding
        hellomsg += "\n"
        hellomsg += "pid: %d" % util.getpid()
        versionmod = sys.modules.get("edenscm.mercurial.__version__")
        if versionmod:
            hellomsg += "\n"
            # pyre-fixme[16]: `ModuleType` has no attribute `versionhash`.
            hellomsg += "versionhash: %s" % versionmod.versionhash
        if util.safehasattr(os, "getpgid"):
            hellomsg += "\n"
            hellomsg += "pgid: %d" % os.getpgid(0)
        # write the hello msg in -one- chunk
        self.cout.write(pycompat.encodeutf8(hellomsg))

        try:
            while self.serveone():
                pass
        except EOFError:
            # we'll get here if the client disconnected while we were reading
            # its request
            return 1

        return 0


def _protectio(ui):
    # type: (Any) -> Tuple[BinaryIO, ...]
    """ duplicates streams and redirect original to null if ui uses stdio """
    ui.flush()
    newfiles = []
    nullfd = os.open(os.devnull, os.O_RDWR)
    for f, sysf, mode in [(ui.fin, util.stdin, "rb"), (ui.fout, util.stdout, "wb")]:
        if f is sysf:
            newfd = os.dup(f.fileno())
            os.dup2(nullfd, f.fileno())
            f = util.fdopen(newfd, mode)
        newfiles.append(f)
    os.close(nullfd)
    return tuple(newfiles)


def _restoreio(ui, fin, fout):
    # type: (Any,BinaryIO,BinaryIO) -> None
    """ restores streams from duplicated ones """
    ui.flush()
    for f, uif in [(fin, ui.fin), (fout, ui.fout)]:
        if f is not uif:
            os.dup2(f.fileno(), uif.fileno())
            f.close()


class pipeservice(object):
    def __init__(self, ui, repo, opts):
        self.ui = ui
        self.repo = repo

    def init(self):
        pass

    def run(self):
        # type: () -> int
        ui = self.ui
        # redirect stdio to null device so that broken extensions or in-process
        # hooks will never cause corruption of channel protocol.
        fin, fout = _protectio(ui)
        try:
            sv = server(ui, self.repo, fin, fout)
            return sv.serve()
        finally:
            sv.cleanup()
            _restoreio(ui, fin, fout)


def _initworkerprocess():
    # type: () -> None
    # use a different process group from the master process, in order to:
    # 1. make the current process group no longer "orphaned" (because the
    #    parent of this process is in a different process group while
    #    remains in a same session)
    #    according to POSIX 2.2.2.52, orphaned process group will ignore
    #    terminal-generated stop signals like SIGTSTP (Ctrl+Z), which will
    #    cause trouble for things like ncurses.
    # 2. the client can use kill(-pgid, sig) to simulate terminal-generated
    #    SIGINT (Ctrl+C) and process-exit-generated SIGHUP. our child
    #    processes like ssh will be killed properly, without affecting
    #    unrelated processes.
    os.setpgid(0, 0)
    # change random state otherwise forked request handlers would have a
    # same state inherited from parent.
    random.seed()


def _serverequest(ui, repo, conn, createcmdserver):
    fin = conn.makefile("rb")
    fout = conn.makefile("wb")
    sv = None
    try:
        sv = createcmdserver(repo, conn, fin, fout)
        try:
            sv.serve()
        # handle exceptions that may be raised by command server. most of
        # known exceptions are caught by dispatch.
        except error.Abort as inst:
            ui.warn(_("abort: %s\n") % inst)
        except IOError as inst:
            if inst.errno != errno.EPIPE:
                raise
        except KeyboardInterrupt:
            pass
        finally:
            sv.cleanup()
    except:  # re-raises
        # print_exc requires a string file-like object in Python 3, so let's get
        # it a buffer then convert it to bytes before sending it to the server.
        output = pycompat.stringutf8io()
        traceback.print_exc(file=output)

        # also write traceback to error channel. otherwise client cannot
        # see it because it is written to server's stderr by default.
        output = pycompat.encodeutf8(output.getvalue())
        if sv:
            sv.cerr.write(output)
        else:
            channeledoutput(fout, b"e").write(output)
        raise
    finally:
        fin.close()
        try:
            fout.close()  # implicit flush() may cause another EPIPE
        except IOError as inst:
            if inst.errno != errno.EPIPE:
                raise


class unixservicehandler(object):
    """Set of pluggable operations for unix-mode services

    Almost all methods except for createcmdserver() are called in the main
    process. You can't pass mutable resource back from createcmdserver().
    """

    pollinterval = None

    def __init__(self, ui):
        self.ui = ui

    def bindsocket(self, sock, address):
        util.bindunixsocket(sock, address)
        sock.listen(socket.SOMAXCONN)
        self.ui.status(_("listening at %s\n") % address)
        self.ui.flush()  # avoid buffering of status message

    def unlinksocket(self, address):
        os.unlink(address)

    def shouldexit(self):
        """True if server should shut down; checked per pollinterval"""
        return False

    def newconnection(self):
        """Called when main process notices new connection"""

    def createcmdserver(self, repo, conn, fin, fout):
        """Create new command server instance; called in the process that
        serves for the current connection"""
        return server(self.ui, repo, fin, fout)


class unixforkingservice(object):
    """
    Listens on unix domain socket and forks server per connection
    """

    def __init__(self, ui, repo, opts, handler=None):
        self.ui = ui
        self.repo = repo
        self.address = opts["address"]
        if not util.safehasattr(socket, "AF_UNIX"):
            raise error.Abort(_("unsupported platform"))
        if not self.address:
            raise error.Abort(_("no socket path specified with --address"))
        self._servicehandler = handler or unixservicehandler(ui)
        self._sock = None
        self._oldsigchldhandler = None
        self._workerpids = set()  # updated by signal handler; do not iterate
        self._socketunlinked = None

    def init(self):
        self._sock = socket.socket(socket.AF_UNIX)
        try:
            import fcntl

            fcntl.FD_CLOEXEC
        except (ImportError, AttributeError):
            pass
        else:
            flags = fcntl.fcntl(self._sock.fileno(), fcntl.F_GETFD)
            flags |= fcntl.FD_CLOEXEC
            fcntl.fcntl(self._sock.fileno(), fcntl.F_SETFD, flags)
        self._servicehandler.bindsocket(self._sock, self.address)
        if util.safehasattr(util, "unblocksignal"):
            util.unblocksignal(signal.SIGCHLD)
        o = signal.signal(signal.SIGCHLD, self._sigchldhandler)
        self._oldsigchldhandler = o
        self._socketunlinked = False

    def _unlinksocket(self):
        if not self._socketunlinked:
            self._servicehandler.unlinksocket(self.address)
            self._socketunlinked = True

    def _cleanup(self):
        signal.signal(signal.SIGCHLD, self._oldsigchldhandler)
        self._sock.close()
        self._unlinksocket()
        # don't kill child processes as they have active clients, just wait
        self._reapworkers(0)

    def run(self):
        try:
            self._mainloop()
        finally:
            self._cleanup()

    def _mainloop(self):
        exiting = False
        h = self._servicehandler
        selector = selectors2.DefaultSelector()
        selector.register(self._sock, selectors2.EVENT_READ)
        while True:
            if not exiting and h.shouldexit():
                # clients can no longer connect() to the domain socket, so
                # we stop queuing new requests.
                # for requests that are queued (connect()-ed, but haven't been
                # accept()-ed), handle them before exit. otherwise, clients
                # waiting for recv() will receive ECONNRESET.
                self._unlinksocket()
                exiting = True
            ready = selector.select(timeout=h.pollinterval)
            if not ready:
                # only exit if we completed all queued requests
                if exiting:
                    break
                continue
            try:
                conn, _addr = self._sock.accept()
            except socket.error as inst:
                if inst.args[0] == errno.EINTR:
                    continue
                raise

            pid = os.fork()
            if pid:
                try:
                    self.ui.debug("forked worker process (pid=%d)\n" % pid)
                    self._workerpids.add(pid)
                    h.newconnection()
                finally:
                    conn.close()  # release handle in parent process
            else:
                try:
                    selector.close()
                    self._sock.close()
                    self._runworker(conn)
                    conn.close()
                    os._exit(0)
                except:  # never return, hence no re-raises
                    try:
                        self.ui.traceback(force=True)
                    finally:
                        os._exit(255)
        selector.close()

    def _sigchldhandler(self, signal, frame):
        self._reapworkers(os.WNOHANG)

    def _reapworkers(self, options):
        while self._workerpids:
            try:
                pid, _status = os.waitpid(-1, options)
            except OSError as inst:
                if inst.errno == errno.EINTR:
                    continue
                if inst.errno != errno.ECHILD:
                    raise
                # no child processes at all (reaped by other waitpid()?)
                self._workerpids.clear()
                return
            if pid == 0:
                # no waitable child processes
                return
            self.ui.debug("worker process exited (pid=%d)\n" % pid)
            self._workerpids.discard(pid)

    def _runworker(self, conn):
        signal.signal(signal.SIGCHLD, self._oldsigchldhandler)
        _initworkerprocess()
        h = self._servicehandler
        try:
            _serverequest(self.ui, self.repo, conn, h.createcmdserver)
        finally:
            gc.collect()  # trigger __del__ since worker process uses os._exit
