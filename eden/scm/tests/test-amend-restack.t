#debugruntest-compatible

# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

# Set up test environment.

  $ eagerepo
  $ setconfig format.use-segmented-changelog=true
  $ setconfig devel.segmented-changelog-rev-compat=true
  $ mkcommit() {
  >   echo $1 > $1
  >   hg ci -m "add $1" -A $1
  > }


  $ enable amend rebase remotenames
  $ setconfig experimental.narrow-heads=True
  $ setconfig visibility.enabled=true mutation.record=true mutation.enabled=true mutation.date='0 0' experimental.evolution= remotenames.rename.default=remote
  $ hg init restack
  $ cd restack

# Note: Repositories populated by `hg debugbuilddag` don't seem to
# correctly show all commits in the log output. Manually creating the
# commits results in the expected behavior, so commits are manually
# created in the test cases below.
# Test unsupported flags:

  $ hg rebase --restack --rev .
  abort: cannot use both --rev and --restack
  [255]
  $ hg rebase --restack --source .
  abort: cannot use both --source and --restack
  [255]
  $ hg rebase --restack --base .
  abort: cannot use both --base and --restack
  [255]
  $ hg rebase --restack --abort
  abort: cannot use both --abort and --restack
  [255]
  $ hg rebase --restack --continue
  abort: cannot use both --continue and --restack
  [255]
  $ hg rebase --restack --hidden
  abort: cannot use both --hidden and --restack
  [255]

# Test basic case of a single amend in a small stack.

  $ mkcommit SRC
  $ hg go -q null

  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ mkcommit d
  $ hg up 'desc("add b")'
  0 files updated, 0 files merged, 2 files removed, 0 files unresolved
  $ echo b >> b
  $ hg amend
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ showgraph
  @  743396f58c5c add b
  │
  │ o  47d2a3944de8 add d
  │ │
  │ o  4538525df7e2 add c
  │ │
  │ x  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  
  o  b22283d0199d add SRC
  $ hg rebase --restack
  rebasing 4538525df7e2 "add c"
  rebasing 47d2a3944de8 "add d"
  $ showgraph
  o  228a9d754739 add d
  │
  o  6d61804ea72c add c
  │
  @  743396f58c5c add b
  │
  o  1f0dee641bb7 add a
  
  o  b22283d0199d add SRC

# Test multiple amends of same commit.

  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ hg up 1
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  $ showgraph
  o  4538525df7e2 add c
  │
  @  7c3bad9141dc add b
  │
  o  1f0dee641bb7 add a

  $ echo b >> b
  $ hg amend
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ echo b >> b
  $ hg amend
  $ showgraph
  @  af408d76932d add b
  │
  │ o  4538525df7e2 add c
  │ │
  │ x  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ hg rebase --restack
  rebasing 4538525df7e2 "add c"
  $ showgraph
  o  e5f1b912c5fa add c
  │
  @  af408d76932d add b
  │
  o  1f0dee641bb7 add a

# Test conflict during rebasing.

  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ mkcommit d
  $ mkcommit e
  $ hg up 1
  0 files updated, 0 files merged, 3 files removed, 0 files unresolved
  $ echo conflict > d
  $ hg add d
  $ hg amend
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ showgraph
  @  e067a66b3532 add b
  │
  │ o  9d206ffc875e add e
  │ │
  │ o  47d2a3944de8 add d
  │ │
  │ o  4538525df7e2 add c
  │ │
  │ x  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ hg rebase --restack
  rebasing 4538525df7e2 "add c"
  rebasing 47d2a3944de8 "add d"
  merging d
  warning: 1 conflicts while merging d! (edit, then use 'hg resolve --mark')
  unresolved conflicts (see hg resolve, then hg rebase --continue)
  [1]
  $ hg rebase --restack
  abort: rebase in progress
  (use 'hg rebase --continue' to continue or
       'hg rebase --abort' to abort)
  [255]
  $ echo merged > d
  $ hg resolve --mark d
  (no more unresolved files)
  continue: hg rebase --continue
  $ hg rebase --continue
  already rebased 4538525df7e2 "add c" as 217450801891
  rebasing 47d2a3944de8 "add d"
  rebasing 9d206ffc875e "add e"
  $ showgraph
  o  b706583c96e3 add e
  │
  o  e247890f1a49 add d
  │
  o  217450801891 add c
  │
  @  e067a66b3532 add b
  │
  o  1f0dee641bb7 add a

# Test finding a stable base commit from within the old stack.

  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ mkcommit d
  $ hg up 1
  0 files updated, 0 files merged, 2 files removed, 0 files unresolved
  $ echo b >> b
  $ hg amend
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ hg up 3
  3 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  o  743396f58c5c add b
  │
  │ @  47d2a3944de8 add d
  │ │
  │ o  4538525df7e2 add c
  │ │
  │ x  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ hg rebase --restack
  rebasing 4538525df7e2 "add c"
  rebasing 47d2a3944de8 "add d"
  $ showgraph
  @  228a9d754739 add d
  │
  o  6d61804ea72c add c
  │
  o  743396f58c5c add b
  │
  o  1f0dee641bb7 add a

# Test finding a stable base commit from a new child of the amended commit.

  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ mkcommit d
  $ hg up 1
  0 files updated, 0 files merged, 2 files removed, 0 files unresolved
  $ echo b >> b
  $ hg amend
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ mkcommit e
  $ showgraph
  @  58e16e5d23eb add e
  │
  o  743396f58c5c add b
  │
  │ o  47d2a3944de8 add d
  │ │
  │ o  4538525df7e2 add c
  │ │
  │ x  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ hg rebase --restack
  rebasing 4538525df7e2 "add c"
  rebasing 47d2a3944de8 "add d"
  $ showgraph
  o  228a9d754739 add d
  │
  o  6d61804ea72c add c
  │
  │ @  58e16e5d23eb add e
  ├─╯
  o  743396f58c5c add b
  │
  o  1f0dee641bb7 add a

# Test finding a stable base commit when there are multiple amends and
# a commit on top of one of the obsolete intermediate commits.

  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ mkcommit d
  $ hg up 1
  0 files updated, 0 files merged, 2 files removed, 0 files unresolved
  $ echo b >> b
  $ hg amend
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ mkcommit e
  $ hg prev
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  [*] add b (glob)
  $ echo b >> b
  $ hg amend
  hint[amend-restack]: descendants of 743396f58c5c are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ hg up 5
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  o  af408d76932d add b
  │
  │ @  58e16e5d23eb add e
  │ │
  │ x  743396f58c5c add b
  ├─╯
  │ o  47d2a3944de8 add d
  │ │
  │ o  4538525df7e2 add c
  │ │
  │ x  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ hg rebase --restack
  rebasing 4538525df7e2 "add c"
  rebasing 47d2a3944de8 "add d"
  rebasing 58e16e5d23eb "add e"
  $ showgraph
  @  2220f78c83d8 add e
  │
  │ o  d61d8c7f922c add d
  │ │
  │ o  e5f1b912c5fa add c
  ├─╯
  o  af408d76932d add b
  │
  o  1f0dee641bb7 add a

# Test that we start from the bottom of the stack. (Previously, restack would
# only repair the unstable children closest to the current changeset. This
# behavior is now incorrect -- restack should always fix the whole stack.)

  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ mkcommit d
  $ hg up 1
  0 files updated, 0 files merged, 2 files removed, 0 files unresolved
  $ echo b >> b
  $ hg amend
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ hg up 2
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo c >> c
  $ hg amend
  hint[amend-restack]: descendants of 4538525df7e2 are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ hg up 3
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  o  dd2a887139a3 add c
  │
  │ o  743396f58c5c add b
  │ │
  │ │ @  47d2a3944de8 add d
  │ │ │
  │ │ x  4538525df7e2 add c
  ├───╯
  x │  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ hg rebase --restack
  rebasing dd2a887139a3 "add c"
  rebasing 47d2a3944de8 "add d"
  $ showgraph
  @  4e2bc7d6cfea add d
  │
  o  afa76d04eaa3 add c
  │
  o  743396f58c5c add b
  │
  o  1f0dee641bb7 add a

# Test what happens if there is no base commit found. The command should
# fix up everything above the current commit, leaving other commits
# below the current commit alone.

  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ mkcommit d
  $ mkcommit e
  $ hg up 3
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  $ echo d >> d
  $ hg amend
  hint[amend-restack]: descendants of 47d2a3944de8 are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ hg up 0
  0 files updated, 0 files merged, 3 files removed, 0 files unresolved
  $ mkcommit f
  $ hg up 1
  1 files updated, 0 files merged, 1 files removed, 0 files unresolved
  $ showgraph
  o  79bfbab36011 add f
  │
  │ o  f2bf14e1d387 add d
  │ │
  │ │ o  9d206ffc875e add e
  │ │ │
  │ │ x  47d2a3944de8 add d
  │ ├─╯
  │ o  4538525df7e2 add c
  │ │
  │ @  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ hg rebase --restack
  rebasing 9d206ffc875e "add e"
  $ showgraph
  o  a660256c6d2a add e
  │
  │ o  79bfbab36011 add f
  │ │
  o │  f2bf14e1d387 add d
  │ │
  o │  4538525df7e2 add c
  │ │
  @ │  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a

# Test having an unamended commit.

  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ hg prev
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  [*] add b (glob)
  $ echo b >> b
  $ hg amend -m Amended
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ echo b >> b
  $ hg amend -m Unamended
  $ hg unamend
  $ hg up -C 1
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  o  173e12a9f067 Amended
  │
  │ o  4538525df7e2 add c
  │ │
  │ @  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ hg rebase --restack
  rebasing 4538525df7e2 "add c"
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  o  b7aa69de00bb add c
  │
  @  4d1e27c9f82b Unamended
  │
  │ x  173e12a9f067 Amended
  ├─╯
  o  1f0dee641bb7 add a

# Revision 2 "add c" is already stable (not orphaned) so restack does nothing:

  $ hg rebase --restack
  nothing to rebase - empty destination

# Test recursive restacking -- basic case.

  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ mkcommit d
  $ hg up 1
  0 files updated, 0 files merged, 2 files removed, 0 files unresolved
  $ echo b >> b
  $ hg amend
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ hg up 2
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo c >> c
  $ hg amend
  hint[amend-restack]: descendants of 4538525df7e2 are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ hg up 1
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  $ showgraph
  o  dd2a887139a3 add c
  │
  │ o  743396f58c5c add b
  │ │
  │ │ o  47d2a3944de8 add d
  │ │ │
  │ │ x  4538525df7e2 add c
  ├───╯
  @ │  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ hg rebase --restack
  rebasing dd2a887139a3 "add c"
  rebasing 47d2a3944de8 "add d"
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  o  4e2bc7d6cfea add d
  │
  o  afa76d04eaa3 add c
  │
  @  743396f58c5c add b
  │
  o  1f0dee641bb7 add a

# Test recursive restacking -- more complex case. This test is designed to
# to check for a bug encountered if rebasing is performed naively from the
# bottom-up wherein obsolescence information for commits further up the
# stack is lost upon rebasing lower levels.

  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ mkcommit d
  $ hg up 1
  0 files updated, 0 files merged, 2 files removed, 0 files unresolved
  $ echo b >> b
  $ hg amend
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ mkcommit e
  $ mkcommit f
  $ hg prev
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  [*] add e (glob)
  $ echo e >> e
  $ hg amend
  hint[amend-restack]: descendants of 58e16e5d23eb are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ hg up 2
  2 files updated, 0 files merged, 1 files removed, 0 files unresolved
  $ echo c >> c
  $ hg amend
  hint[amend-restack]: descendants of 4538525df7e2 are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ mkcommit g
  $ mkcommit h
  $ hg prev
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  [*] add g (glob)
  $ echo g >> g
  $ hg amend
  hint[amend-restack]: descendants of a063c2736716 are left behind - use 'hg restack' to rebase them
  hint[hint-ack]: use 'hg hint --ack amend-restack' to silence these hints
  $ hg up 1
  0 files updated, 0 files merged, 2 files removed, 0 files unresolved
  $ showgraph
  o  8282a17a7483 add g
  │
  │ o  e86422ad5d0e add h
  │ │
  │ x  a063c2736716 add g
  ├─╯
  o  dd2a887139a3 add c
  │
  │ o  e429b2ca5d8b add e
  │ │
  │ │ o  849d5cce0019 add f
  │ │ │
  │ │ x  58e16e5d23eb add e
  │ ├─╯
  │ o  743396f58c5c add b
  │ │
  │ │ o  47d2a3944de8 add d
  │ │ │
  │ │ x  4538525df7e2 add c
  ├───╯
  @ │  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ hg rebase --restack
  rebasing dd2a887139a3 "add c"
  rebasing 8282a17a7483 "add g"
  rebasing 849d5cce0019 "add f"
  rebasing 47d2a3944de8 "add d"
  rebasing e86422ad5d0e "add h"
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  o  5bc29b84815f add h
  │
  │ o  4e2bc7d6cfea add d
  │ │
  │ │ o  6aaca8e17a00 add f
  │ │ │
  o │ │  c7fc06907e30 add g
  ├─╯ │
  o   │  afa76d04eaa3 add c
  │   │
  │   o  e429b2ca5d8b add e
  ├───╯
  @  743396f58c5c add b
  │
  o  1f0dee641bb7 add a
