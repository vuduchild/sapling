#chg-compatible
  $ setconfig experimental.allowfilepeer=True

Set up extension and repos

  $ eagerepo
  $ setconfig phases.publish=false
  $ setconfig remotenames.rename.default=
For convenience. The test was written without selectivepull and uses the "@" bookmark.
  $ setconfig remotenames.selectivepulldefault=master,@

  $ newclientrepo repo2 test:repo1

Test that anonymous heads are disallowed by default

  $ echo a > a
  $ hg add a
  $ hg commit -m a
  $ hg push
  pushing to test:repo1
  searching for changes
  abort: push would create new anonymous heads (cb9a9f314b8b)
  (use --allow-anon to override this warning)
  [255]

Test that config changes what is pushed by default

  $ echo b > b
  $ hg add b
  $ hg commit -m b
  $ hg up ".^"
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  $ echo c > c
  $ hg add c
  $ hg commit -m c
  $ hg push -r 'head()'
  pushing to test:repo1
  searching for changes
  abort: push would create new anonymous heads (d2ae7f538514, d36c0562f908)
  (use --allow-anon to override this warning)
  [255]
  $ hg push -r .
  pushing to test:repo1
  searching for changes
  abort: push would create new anonymous heads (d36c0562f908)
  (use --allow-anon to override this warning)
  [255]
  $ hg debugstrip d36c0562f908 d2ae7f538514
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved

Test that config allows anonymous heads to be pushed

  $ hg push --config remotenames.pushanonheads=True
  pushing to test:repo1
  searching for changes

Test that forceto works

  $ setglobalconfig remotenames.forceto=true
  $ hg push
  abort: must specify --to when pushing
  (see configuration option remotenames.forceto)
  [255]

Test that --to limits other options

  $ echo b >> a
  $ hg commit -m b
  $ hg push --to @ --rev . --rev ".^"
  abort: --to requires exactly one rev to push
  (use --rev BOOKMARK or omit --rev for current commit (.))
  [255]
  $ hg push --to @ --bookmark foo
  abort: do not specify --to/-t and --bookmark/-B at the same time
  [255]

Test that --create is required to create new bookmarks

  $ hg push --to @
  pushing rev 1846eede8b68 to destination test:repo1 bookmark @
  searching for changes
  abort: not creating new remote bookmark
  (use --create to create a new bookmark)
  [255]
  $ hg push --to @ --create
  pushing rev 1846eede8b68 to destination test:repo1 bookmark @
  searching for changes
  exporting bookmark @

Test that --non-forward-move is required to move bookmarks to odd locations

  $ hg push --to @
  pushing rev 1846eede8b68 to destination test:repo1 bookmark @
  searching for changes
  remote bookmark already points at pushed rev
  $ hg push --to @ -r ".^"
  pushing rev cb9a9f314b8b to destination test:repo1 bookmark @
  searching for changes
  abort: pushed rev is not in the foreground of remote bookmark
  (use --non-forward-move flag to complete arbitrary moves)
  [255]
  $ hg up ".^"
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo c >> a
  $ hg commit -m c
  $ hg push --to @
  pushing rev cc61aa6be3dc to destination test:repo1 bookmark @
  searching for changes
  abort: pushed rev is not in the foreground of remote bookmark
  (use --non-forward-move flag to complete arbitrary moves)
  [255]

Test that --non-forward-move allows moving bookmark around arbitrarily

  $ hg book -r 'desc(b)' headb
  $ hg book -r 'desc(c)' headc
  $ hg log -G -T '{desc} {bookmarks} {remotebookmarks}\n'
  @  c headc
  │
  │ o  b headb default/@
  ├─╯
  o  a
  
  $ hg push --to @ -r headb
  pushing rev 1846eede8b68 to destination test:repo1 bookmark @
  searching for changes
  remote bookmark already points at pushed rev
  $ hg push --to @ -r headb
  pushing rev 1846eede8b68 to destination test:repo1 bookmark @
  searching for changes
  remote bookmark already points at pushed rev
  $ hg push --to @ -r headc
  pushing rev cc61aa6be3dc to destination test:repo1 bookmark @
  searching for changes
  abort: pushed rev is not in the foreground of remote bookmark
  (use --non-forward-move flag to complete arbitrary moves)
  [255]
  $ hg push --to @ -r headc --non-forward-move
  pushing rev cc61aa6be3dc to destination test:repo1 bookmark @
  searching for changes
  updating bookmark @
  $ hg push --to @ -r 'desc(a)'
  pushing rev cb9a9f314b8b to destination test:repo1 bookmark @
  searching for changes
  abort: pushed rev is not in the foreground of remote bookmark
  (use --non-forward-move flag to complete arbitrary moves)
  [255]
  $ hg push --to @ -r 'desc(a)' --non-forward-move
  pushing rev cb9a9f314b8b to destination test:repo1 bookmark @
  searching for changes
  updating bookmark @
  $ hg push --to @ -r headb
  pushing rev 1846eede8b68 to destination test:repo1 bookmark @
  searching for changes
  updating bookmark @

Test that local must have rev of remote to push --to without --non-forward-move

  $ hg up -r 'desc(a)'
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ hg debugstrip -r headb
  $ hg book -d headb
  $ hg push --to @ -r headc
  pushing rev cc61aa6be3dc to destination test:repo1 bookmark @
  searching for changes
  abort: remote bookmark @ revision 1846eede8b6886d8cc8a88c96a687b7fe8f3b9d1 is not in local repo
  (pull and merge or rebase or use --non-forward-move)
  [255]


Test that rebasing and pushing works as expected

  $ hg pull
  pulling from test:repo1
  searching for changes
  $ hg log -G -T '{desc} {bookmarks} {remotebookmarks}\n'
  o  c headc
  │
  │ o  b  default/@
  ├─╯
  @  a
  
  $ hg --config extensions.rebase= rebase -d default/@ -s headc 2>&1 | grep -v "^warning:" | grep -v incomplete
  rebasing cc61aa6be3dc "c" (headc)
  merging a
  unresolved conflicts (see hg resolve, then hg rebase --continue)
  $ echo "a" > a
  $ echo "b" >> a
  $ echo "c" >> a
  $ hg resolve --mark a
  (no more unresolved files)
  $ hg --config extensions.rebase= rebase --continue
  rebasing cc61aa6be3dc "c" (headc)
  $ hg log -G -T '{desc} {bookmarks} {remotebookmarks}\n'
  o  c headc
  │
  o  b  default/@
  │
  @  a
  
  $ hg up headc
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (activating bookmark headc)
  $ hg push --to @
  pushing rev 6683576730c5 to destination test:repo1 bookmark @
  searching for changes
  updating bookmark @
  $ hg log -G -T '{desc} {bookmarks} {remotebookmarks}\n'
  @  c headc default/@
  │
  o  b
  │
  o  a
  
# Evolve related tests removed. see https://fburl.com/evolveeol
