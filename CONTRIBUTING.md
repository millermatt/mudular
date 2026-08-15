# Contributing to Mudular

Patches welcome. Two things to know before you send one.

## Licence

Mudular is **GPL-3.0-or-later** (see [LICENSE](LICENSE)). Anything you
contribute is under the same terms, and stays that way — there is no
contributor licence agreement to sign and no copyright assignment, so nobody
can relicense your work out from under you, including the maintainer.

That is a deliberate trade. It means Mudular can never be dual-licensed or
sold under proprietary terms, and in exchange it can borrow directly from the
GPL clients it sits alongside — TinTin++ is GPL-3.0, which is what makes
importing a TinTin config a port rather than a clean-room rewrite.

## Sign your commits (DCO)

Every commit needs a `Signed-off-by` line, which `git` will add for you:

```sh
git commit -s -m "fix: ..."
```

That line certifies you wrote the patch, or otherwise have the right to submit
it under the project's licence — the [Developer Certificate of
Origin](https://developercertificate.org/), reproduced below. It is not a
copyright assignment.

<details>
<summary>Developer Certificate of Origin 1.1</summary>

```
By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same license (unless I am permitted to submit
    under a different license), as indicated in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

</details>

**Do not paste code you found elsewhere unless you know its licence.** Code
from a GPL-2.0-only project cannot be combined with this one; permissively
licensed code can, with its notice kept.

## Working on it

`docs/ARCHITECTURE.md` is the design authority and explains why the module
boundaries are where they are; `docs/USAGE.md` is the user-facing reference.

Before opening a pull request:

```sh
cargo fmt
cargo clippy --all-targets    # no warnings
cargo test                    # all green
```

Conventional Commits for messages (`feat:`, `fix:`, `docs:`, `chore:`). This
is not cosmetic — the release automation reads them: `feat:` and `fix:`
decide the version bump and open the release PR, and everything else rides
along in the changelog without triggering a release of its own.

Protocol and parser changes come with byte-fixture unit tests; see the pattern
in `src/proto/telnet.rs`. Anything that only shows up in a real terminal has a
harness of its own — see `.claude/skills/run/` and `tests/pty_smoke.rs`.
