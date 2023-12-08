#chg-compatible

#require unix-permissions no-root no-windows

  $ configure modernclient

Prepare

  $ newclientrepo a
  $ cd ..
  $ echo a > a/a
  $ hg -R a ci -A -m a
  adding a
  $ hg -R a push -q --to book --create

  $ newclientrepo b test:a_server book
  $ cd ..

Test that raising an exception in the release function doesn't cause the lock to choke

  $ cat > testlock.py << EOF
  > from sapling import error, registrar
  > 
  > cmdtable = {}
  > command = registrar.command(cmdtable)
  > 
  > def acquiretestlock(repo, releaseexc):
  >     def unlock():
  >         if releaseexc:
  >             raise error.Abort('expected release exception')
  >     l = repo._lock(repo.localvfs, 'testlock', False, unlock, None, 'test lock')
  >     return l
  > 
  > @command('testlockexc')
  > def testlockexc(ui, repo):
  >     testlock = acquiretestlock(repo, True)
  >     try:
  >         testlock.release()
  >     finally:
  >         try:
  >             testlock = acquiretestlock(repo, False)
  >         except error.LockHeld:
  >             raise error.Abort('lockfile on disk even after releasing!')
  >         testlock.release()
  > EOF
  $ cat >> $HGRCPATH << EOF
  > [extensions]
  > testlock=$TESTTMP/testlock.py
  > EOF

  $ hg -R b testlockexc
  abort: expected release exception
  [255]

One process waiting for another for a significant period of time (longer than the default threshold). Warning should be shown.

  $ cat > hooks.py << EOF
  > import time
  > def sleeplong(**x):
  >     import os
  >     os.system("touch sleeping")
  >     time.sleep(2)
  > EOF
  $ echo b > b/b
  $ hg -R b ci -A -m b --config hooks.precommit="python:`pwd`/hooks.py:sleeplong" > stdout &
Wait until bg process has entered critical section.
  $ while [ ! -f sleeping ]; do sleep 0.01; done
  $ hg -R b up -q --config ui.timeout.warn=0 > preup-stdout 2>preup-stderr
  $ wait
  $ cat preup-stdout
  $ cat preup-stderr
  waiting for lock on working directory of b held by process '*' on host '*' (glob)
  (hint: run 'hg debugprocesstree *' to see related processes) (glob)
  got lock after * seconds (glob) (?)
  $ cat stdout
  adding b

One process waiting for another for short period of time. No warning.

  $ cat > hooks.py << EOF
  > import time
  > def sleepone(**x): time.sleep(1)
  > def sleephalf(**x): time.sleep(0.5)
  > EOF
  $ echo b > b/c
  $ hg -R b ci -A -m b --config hooks.precommit="python:`pwd`/hooks.py:sleepone" > stdout &
  $ hg -R b up -q --config hooks.pre-update="python:`pwd`/hooks.py:sleephalf" \
  > > preup-stdout 2>preup-stderr
  $ wait
  $ cat preup-stdout
  $ cat preup-stderr
  $ cat stdout
  adding c

On processs waiting on another, warning after a long time.

  $ echo b > b/d
  $ hg -R b ci -A -m b --config hooks.precommit="python:`pwd`/hooks.py:sleepone" > stdout &
  $ hg -R b up -q --config hooks.pre-update="python:`pwd`/hooks.py:sleephalf" \
  > --config ui.timeout.warn=250 \
  > > preup-stdout 2>preup-stderr
  $ wait
  $ cat preup-stdout
  $ cat preup-stderr
  $ cat stdout
  adding d

On processs waiting on another, warning disabled.

  $ echo b > b/e
  $ hg -R b ci -A -m b --config hooks.precommit="python:`pwd`/hooks.py:sleepone" > stdout &
  $ hg -R b up -q --config hooks.pre-update="python:`pwd`/hooks.py:sleephalf" \
  > --config ui.timeout.warn=-1 \
  > > preup-stdout 2>preup-stderr
  $ wait
  $ cat preup-stdout
  $ cat preup-stderr
  $ cat stdout
  adding e

check we still print debug output

On processs waiting on another, warning after a long time (debug output on)

  $ echo b > b/f
  $ hg -R b ci -A -m b --config hooks.precommit="python:`pwd`/hooks.py:sleepone" > stdout &
  $ hg -R b up --config hooks.pre-update="python:`pwd`/hooks.py:sleephalf" \
  > --config ui.timeout.warn=250 --debug\
  > > preup-stdout 2>preup-stderr
  $ wait
  $ cat preup-stdout
  calling hook pre-update: hghook_pre-update.sleephalf
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cat preup-stderr
  waiting for lock on working directory of b held by process '*' on host * (glob)
  (hint: run * to see related processes) (glob)
  got lock after * seconds (glob) (?)
  $ cat stdout
  adding f

On processs waiting on another, warning disabled, (debug output on)

  $ echo b > b/g
  $ hg -R b ci -A -m b --config hooks.precommit="python:`pwd`/hooks.py:sleepone" > stdout &
  $ hg -R b up --config hooks.pre-update="python:`pwd`/hooks.py:sleephalf" \
  > --config ui.timeout.warn=-1 --debug\
  > > preup-stdout 2>preup-stderr
  $ wait
  $ cat preup-stdout
  calling hook pre-update: hghook_pre-update.sleephalf
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cat preup-stderr
  waiting for lock on working directory of b held by process '*' on host * (glob)
  (hint: run * to see related processes) (glob)
  got lock after * seconds (glob) (?)
  $ cat stdout
  adding g

Having an empty lock file
  $ cd a
  $ touch .hg/wlock
  $ hg backout # a command which always acquires a lock
  abort: please specify a revision to backout
  [255]

Non-symlink stale lock is removed automatically.

Having an empty undolog lock file
  $ mkdir .hg/undolog && touch .hg/undolog/lock
  $ hg debuglocks
  lock:          free
  wlock:         free
  undolog/lock:  free
  prefetchlock:  free
  infinitepushbackup.lock: free
  $ hg debuglocks --force-undolog-lock
  abort: forcing lock release no longer supported
  [255]
  $ hg debuglocks
  lock:          free
  wlock:         free
  undolog/lock:  free
  prefetchlock:  free
  infinitepushbackup.lock: free

  $ cd ..
