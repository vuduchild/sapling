#debugruntest-compatible

  $ eagerepo
test that a commit clears the merge state.

  $ hg init repo
  $ cd repo

  $ echo foo > file1
  $ echo foo > file2
  $ hg commit -Am 'add files'
  adding file1
  adding file2

  $ echo bar >> file1
  $ echo bar >> file2
  $ hg commit -Am 'append bar to files'

create a second head with conflicting edits

  $ hg up -C 'desc(add)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo baz >> file1
  $ echo baz >> file2
  $ hg commit -Am 'append baz to files'

create a third head with no conflicting edits
  $ hg up -qC 'desc(add)'
  $ echo foo > file3
  $ hg commit -Am 'add non-conflicting file'
  adding file3

failing merge

  $ hg up -qC 'max(desc(append))'
  $ hg merge --tool=internal:fail dc77451844e37f03f5c559e3b8529b2b48d381d1
  0 files updated, 0 files merged, 0 files removed, 2 files unresolved
  use 'hg resolve' to retry unresolved file merges or 'hg goto -C .' to abandon
  [1]

resolve -l should contain unresolved entries

  $ hg resolve -l
  U file1
  U file2

  $ hg resolve -l --no-status
  file1
  file2

resolving an unknown path should emit a warning, but not for -l

  $ hg resolve -m does-not-exist
  arguments do not match paths that need resolving
  $ hg resolve -l does-not-exist

tell users how they could have used resolve

  $ mkdir nested
  $ cd nested
  $ hg resolve -m file1
  arguments do not match paths that need resolving
  (try: hg resolve -m path:file1)
  $ hg resolve -m file1 filez
  arguments do not match paths that need resolving
  (try: hg resolve -m path:file1 path:filez)
  $ hg resolve -m path:file1 path:filez

test --root-relative
  $ hg resolve -l
  R ../file1
  U ../file2
  $ hg resolve -l --no-root-relative
  R ../file1
  U ../file2
  $ hg resolve -l --root-relative
  R file1
  U file2
  $ HGPLAIN=1 hg resolve -l
  R file1
  U file2
  $ HGPLAIN=1 hg resolve -l --no-root-relative
  R ../file1
  U ../file2
  $ HGPLAIN=1 hg resolve -l --root-relative
  R file1
  U file2

continue resolving the conflict
  $ hg resolve -m filez file2
  arguments do not match paths that need resolving
  (try: hg resolve -m path:filez path:file2)
  $ hg resolve -m path:filez path:file2
  (no more unresolved files)
  $ hg resolve -l
  R ../file1
  R ../file2

cleanup
  $ hg resolve -u
  $ cd ..
  $ rmdir nested

don't allow marking or unmarking driver-resolved files

  $ cat > $TESTTMP/markdriver.py << EOF
  > '''mark and unmark files as driver-resolved'''
  > from sapling import merge, registrar, scmutil
  > cmdtable = {}
  > command = registrar.command(cmdtable)
  > @command('markdriver',
  >   [('u', 'unmark', None, '')],
  >   'FILE...')
  > def markdriver(ui, repo, *pats, **opts):
  >     wlock = repo.wlock()
  >     try:
  >         ms = merge.mergestate.read(repo)
  >         m = scmutil.match(repo[None], pats, opts)
  >         for f in ms:
  >             if not m(f):
  >                 continue
  >             if not opts['unmark']:
  >                 ms.mark(f, 'd')
  >             else:
  >                 ms.mark(f, 'u')
  >         ms.commit()
  >     finally:
  >         wlock.release()
  > EOF
  $ hg --config extensions.markdriver=$TESTTMP/markdriver.py markdriver file1
  $ hg resolve --list
  D file1
  U file2
  $ hg resolve --mark file1
  not marking file1 as it is driver-resolved
this should not print out file1
  $ hg resolve --mark --all
  (no more unresolved files -- run "hg resolve --all" to conclude)
  $ hg resolve --mark 'glob:file*'
  (no more unresolved files -- run "hg resolve --all" to conclude)
  $ hg resolve --list
  D file1
  R file2
  $ hg resolve --unmark file1
  not unmarking file1 as it is driver-resolved
  (no more unresolved files -- run "hg resolve --all" to conclude)
  $ hg resolve --unmark --all
  $ hg resolve --list
  D file1
  U file2
  $ hg --config extensions.markdriver=$TESTTMP/markdriver.py markdriver --unmark file1
  $ hg resolve --list
  U file1
  U file2

resolve the failure

  $ echo resolved > file1
  $ hg resolve -m file1

resolve -l should show resolved file as resolved

  $ hg resolve -l
  R file1
  U file2

  $ hg resolve -l -Tjson
  [
   {
    "path": "file1",
    "status": "R"
   },
   {
    "path": "file2",
    "status": "U"
   }
  ]

resolve -m without paths should mark all resolved

  $ hg resolve -m
  (no more unresolved files)
  $ hg commit -m 'resolved'

resolve -l should be empty after commit

  $ hg resolve -l

  $ hg resolve -l -Tjson
  [
  ]

resolve --all should abort when no merge in progress

  $ hg resolve --all
  abort: resolve command not applicable when not merging
  [255]

resolve -m should abort when no merge in progress

  $ hg resolve -m
  abort: resolve command not applicable when not merging
  [255]

can not update or merge when there are unresolved conflicts

  $ hg up -qC 99726c03216e233810a2564cbc0adfe395007eac
  $ echo quux >> file1
  $ hg up dc77451844e37f03f5c559e3b8529b2b48d381d1
  merging file1
  warning: 1 conflicts while merging file1! (edit, then use 'hg resolve --mark')
  1 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'hg resolve' to retry unresolved file merges
  [1]
  $ hg up 99726c03216e233810a2564cbc0adfe395007eac
  abort: unresolved merge state
  (use 'hg resolve' to continue or
       'hg goto --clean' to abort - WARNING: will destroy uncommitted changes)
  [255]
  $ hg merge 'max(desc(append))'
  abort: outstanding merge conflicts
  [255]

set up conflict-free merge

  $ hg up -qC 'max(desc(add))'
  $ hg merge dc77451844e37f03f5c559e3b8529b2b48d381d1
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (branch merge, don't forget to commit)

resolve --all should do nothing in merge without conflicts
  $ hg resolve --all
  (no more unresolved files)

resolve -m should do nothing in merge without conflicts

  $ hg resolve -m
  (no more unresolved files)

get back to conflicting state

  $ hg up -qC 'max(desc(append))'
  $ hg merge --tool=internal:fail dc77451844e37f03f5c559e3b8529b2b48d381d1
  0 files updated, 0 files merged, 0 files removed, 2 files unresolved
  use 'hg resolve' to retry unresolved file merges or 'hg goto -C .' to abandon
  [1]

resolve without arguments should suggest --all
  $ hg resolve
  abort: no files or directories specified
  (use --all to re-merge all unresolved files)
  [255]

resolve --all should re-merge all unresolved files
  $ hg resolve --all
  merging file1
  merging file2
  warning: 1 conflicts while merging file1! (edit, then use 'hg resolve --mark')
  warning: 1 conflicts while merging file2! (edit, then use 'hg resolve --mark')
  [1]
  $ cat file1.orig
  foo
  baz
  $ cat file2.orig
  foo
  baz

.orig files should exists where specified
  $ hg resolve --all --verbose --config 'ui.origbackuppath=.hg/origbackups'
  merging file1
  creating directory: $TESTTMP/repo/.hg/origbackups
  merging file2
  warning: 1 conflicts while merging file1! (edit, then use 'hg resolve --mark')
  warning: 1 conflicts while merging file2! (edit, then use 'hg resolve --mark')
  [1]
  $ ls .hg/origbackups
  file1
  file2
  $ grep '<<<' file1 > /dev/null
  $ grep '<<<' file2 > /dev/null

resolve <file> should re-merge file
  $ echo resolved > file1
  $ hg resolve -q file1
  warning: 1 conflicts while merging file1! (edit, then use 'hg resolve --mark')
  [1]
  $ grep '<<<' file1 > /dev/null

test .orig behavior with resolve

  $ hg resolve -q file1 --tool "sh -c 'f --dump \"$TESTTMP/repo/file1.orig\"'"
  $TESTTMP/repo/file1.orig:
  >>>
  foo
  baz
  <<<

resolve <file> should do nothing if 'file' was marked resolved
  $ echo resolved > file1
  $ hg resolve -m file1
  $ hg resolve -q file1
  $ cat file1
  resolved

insert unsupported advisory merge record

  $ hg debugmergestate --add-unsupported-advisory-record
  $ hg debugmergestate
  local: 57653b9f834a4493f7240b0681efcb9ae7cab745
  other: dc77451844e37f03f5c559e3b8529b2b48d381d1
  labels:
    local: working copy
    other: merge rev
  file: file1 (record type "F", state "r", hash 60b27f004e454aca81b0480209cce5081ec52390)
    local path: file1 (flags "")
    ancestor path: file1 (node 2ed2a3912a0b24502043eae84ee4b279c18b90dd)
    other path: file1 (node 6f4310b00b9a147241b071a60c28a650827fb03d)
    extras: ancestorlinknode=99726c03216e233810a2564cbc0adfe395007eac
  file: file2 (record type "F", state "u", hash cb99b709a1978bd205ab9dfd4c5aaa1fc91c7523)
    local path: file2 (flags "")
    ancestor path: file2 (node 2ed2a3912a0b24502043eae84ee4b279c18b90dd)
    other path: file2 (node 6f4310b00b9a147241b071a60c28a650827fb03d)
    extras: ancestorlinknode=99726c03216e233810a2564cbc0adfe395007eac
  unsupported record "x" (data ["advisory record"])
  $ hg resolve -l
  R file1
  U file2

insert unsupported mandatory merge record

  $ hg debugmergestate --add-unsupported-mandatory-record
  $ hg debugmergestate
  local: 57653b9f834a4493f7240b0681efcb9ae7cab745
  other: dc77451844e37f03f5c559e3b8529b2b48d381d1
  labels:
    local: working copy
    other: merge rev
  file: file1 (record type "F", state "r", hash 60b27f004e454aca81b0480209cce5081ec52390)
    local path: file1 (flags "")
    ancestor path: file1 (node 2ed2a3912a0b24502043eae84ee4b279c18b90dd)
    other path: file1 (node 6f4310b00b9a147241b071a60c28a650827fb03d)
    extras: ancestorlinknode=99726c03216e233810a2564cbc0adfe395007eac
  file: file2 (record type "F", state "u", hash cb99b709a1978bd205ab9dfd4c5aaa1fc91c7523)
    local path: file2 (flags "")
    ancestor path: file2 (node 2ed2a3912a0b24502043eae84ee4b279c18b90dd)
    other path: file2 (node 6f4310b00b9a147241b071a60c28a650827fb03d)
    extras: ancestorlinknode=99726c03216e233810a2564cbc0adfe395007eac
  unsupported record "X" (data ["mandatory record"])
  $ hg resolve -l
  abort: unsupported merge state records: X
  (see https://mercurial-scm.org/wiki/MergeStateRecords for more information)
  [255]
  $ hg resolve -ma
  abort: unsupported merge state records: X
  (see https://mercurial-scm.org/wiki/MergeStateRecords for more information)
  [255]
  $ hg summary
  warning: merge state has unsupported record types: X
  parent: 57653b9f834a 
   append baz to files
  parent: dc77451844e3 
   append bar to files
  commit: 2 modified, 2 unknown (merge)
  phases: 5 draft

goto --clean shouldn't abort on unsupported records

  $ hg up -qC dc77451844e37f03f5c559e3b8529b2b48d381d1
  $ hg debugmergestate
  no merge state found

test crashed merge with empty mergestate

  $ mkdir .hg/merge
  $ touch .hg/merge/state2

resolve -l should be empty

  $ hg resolve -l

  $ cd ..
