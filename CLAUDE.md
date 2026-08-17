# Mudular — project instructions

Terminal MUD client in Rust (ratatui + tokio). Design authority is
docs/ARCHITECTURE.md; build order is its §14 roadmap; per-milestone model
guidance is docs/DEVELOPMENT.md.

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
it is two branches. This is a rule about `main` as much as about branches: PRs
are squash-merged, so a branch becomes exactly one commit and exactly one
changelog line. A branch carrying a feature plus four unrelated fixes lands as
one commit whose subject misdescribes four of them, and their explanations end
up filed under something they have nothing to do with. PR #72 was that mistake
— a workflow plus four harness bugs, squashed into `ci: …` — and squashing is
what made it painless to create, which is why the discipline has to be here.

Corollary: when a fix turns up mid-branch and is not what the branch is about,
finish the branch, then do the fix in its own. Two small PRs beat one honest
commit message apologising for a mixed one.

**This checkout is habitually behind `origin/main`.** PRs — including
release-plz's release PRs — are merged on GitHub, which never touches the local
repo. So:

- `git fetch && git checkout main && git pull --ff-only` **before cutting a
  branch**, every time. A branch cut from a stale tip cannot fast-forward, and
  the failure surfaces later as a rejected push or a PR that needs a rebase.
- Rebase onto `main` rather than merging it back in, and force-push with
  `--force-with-lease`.
- Never `git pull` without `--ff-only`: an accidental merge commit on `main`
  clutters the changelog release-plz generates from these commits.

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
