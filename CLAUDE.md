# Mudular — project instructions

Terminal MUD client in Rust (ratatui + tokio). Design authority is
docs/ARCHITECTURE.md; build order is its §14 roadmap; who a decision is for is
docs/ACTORS.md; per-milestone model guidance is docs/DEVELOPMENT.md.

## Working on a milestone

- Before coding, read the milestone's row in ARCHITECTURE.md §14 and the TAD
  sections it references. The "done when" column is the acceptance criterion —
  the milestone is finished when it holds, not before, not beyond.
- Respect the module boundaries (TAD §4): `proto` and `engine` are sans-IO and
  depend on nothing above them; `session` composes them; `ui` never touches
  sockets. Don't cross these to save a few lines.
- Protocol/parser changes come with byte-fixture unit tests — follow the
  pattern in src/proto/telnet.rs.
- Prefer established crates per TAD §2.1. First-party code is only for the
  Telnet/MCCP/GMCP/MSDP core.

## Scope discipline

- Build only the current milestone. Later-milestone stubs stay stubs; leave
  seams, not speculative features.
- No new dependencies, abstractions, or config options beyond what the
  milestone needs.

## Who the change is for

ARCHITECTURE.md answers *how*. docs/ACTORS.md answers *who for*: two
invariants that hold for every change, and the people who play — multi-boxer,
solo player, screen-reader user, module author, newcomer, power-user
scripter. Read it alongside §14, not only when reading UX_REVIEW.md.

- **Name the player in an issue or PR that proposes new surface area.** Not
  ceremony: saying who a change is for is what makes it arguable at all, and
  it is how a feature nobody needs gets caught before it is built rather than
  after.
- **The list is not a ranking, and there is nothing to win.** Precedence
  applies in exactly one place — a default, which cannot be split between two
  people, goes to the multi-boxer. Everywhere else a preference, a mode or a
  command can serve one player without being taken from another, so "which
  player outranks which" is the wrong question. Don't invent a contest to
  justify a change; say who it helps and what it costs.
- **An invariant is never the side that loses.** Module and script isolation,
  and local data being owner-only, are not traded for anyone's convenience —
  a design that needs one relaxed needs redesigning instead.
- **A change that serves nobody on the list is one to question before
  building, not after.** ACTORS.md says this itself, and it is the same rule
  as scope discipline above. It is a prompt rather than a veto: a real player
  the list fails to describe is a reason to fix the list.
- Bug fixes, refactors and test work don't need this. It is for new surface
  area: features, config keys, client commands, panes.

## Commands

- `cargo test` — must pass
- `cargo clippy --all-targets` — no warnings
- `cargo fmt` — run before committing
- Conventional Commits for commit messages

## Git

**Never push to `main`.** Every change goes on a branch and through a pull
request, however small — a one-line README fix included. Branch protection
allows a direct push, deliberately, so that a human can make one; that
permission is not for you. A PR is what gets the change tested on all three
platforms before it is on `main`, and CI is the only thing that runs macOS and
Windows at all.

**One concern per branch.** If the change cannot be described in one sentence,
it is two branches. This is a rule about `main` as much as about branches:
squash merging is off, so **every commit on a branch lands on `main`** and
release-plz reads each one when it builds the changelog. A branch carrying a
feature plus four unrelated fixes files those fixes under a release note
about the feature, where nobody looking for them will find them.

Corollary: when a fix turns up mid-branch and is not what the branch is about,
finish the branch, then do the fix in its own. Two small PRs beat one whose
history has to be read to work out what it did.

**The PR title is the release note, and the release trigger.** A merge commit
takes the PR title as its subject, and that subject is what release-plz reads
and matches against `release_commits` in `release-plz.toml`. So a PR titled
`refactor:` or `docs:` ships no release however many `feat:` commits it
contains, and a PR titled `feat:` ships one however small it is. Title the PR
for what the whole branch does, in Conventional Commits form.

Commits inside the branch are for the reviewer and for whoever is bisecting
later. They land on `main` and they are worth writing well, but they are not
what the changelog is built from — do not count on one of them reaching a
release note, and do not assume a `fix:` among them will trigger a release.

**This checkout is habitually behind `origin/main`.** PRs — including
release-plz's release PRs — are merged on GitHub, which never touches the local
repo. So:

- `git fetch && git checkout main && git pull --ff-only` **before cutting a
  branch**, every time. A branch cut from a stale tip cannot fast-forward, and
  the failure surfaces later as a rejected push or a PR that needs a rebase.
- Rebase onto `main` rather than merging it back in, and force-push with
  `--force-with-lease`. **Rebase again if `main` moved while the PR was
  open.** A branch merged while it is behind the base can have its commits
  dropped from the changelog entirely — silently, exit code 0 — so this is
  load-bearing for releases and not only for tidy history.
- Never `git pull` without `--ff-only`: an accidental merge commit on `main`
  clutters the changelog release-plz generates from these commits.

**Don't merge the release PR while a Release-plz run is in flight.** The
workflow now cancels its own superseded PR runs, so a stale changelog can no
longer land on its own. What it cannot guard is a merge landing between a run
reading "is there an open release PR?" and acting on the answer — the racing
party is the person clicking merge. That produced the duplicate v0.6.1 PR.
Merging is the irreversible step, since it tags and dist builds from the tag.

Test names, doc comments and commit bodies carry the *reason* here, not just
the change. When a fix is subtle, the comment explaining why is part of it.

## Don't make the tests the thing that has to remember

Two footguns recurred often enough to be worth naming, both now guarded in
code rather than by instruction — keep them that way:

- **Terminal sizes in `ui` tests are derived, not tuned.** A hand-picked width
  or height that "fits the help listing" silently stops fitting the next time
  a row is added, and the failure reads as a rendering bug.
  See `overlay_fits`, and `no_help_row_is_too_long_for_an_eighty_column_terminal`.
- **`Esc`-prefixed keys in `tests/pty_smoke.rs` go through `press_esc`.** An
  `Esc` byte in the same pty read as the bytes before it parses as one
  Alt-modified key. Raw `\x1b` sends pass alone and fail under load.

Sibling repos are not ours to edit. `../hercmud` in particular: read it freely,
but findings go to that repo's own session.
