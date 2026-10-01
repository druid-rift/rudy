# Contributing

Rudy writes to disks, so nothing reaches `master` on trust.

**Every pull request is reviewed and approved by the maintainer before it is merged.**
The branch is protected: a merge needs an approving review from someone with write
access, the `linux` CI job green on the latest commit, every review thread resolved,
and the approval given after the last push. CI on a first-time contributor's pull
request waits for the maintainer to start it.

## Before you open one

1. Open an issue first for anything larger than a small fix, so the change is agreed
   before it is written. Unsolicited pull requests with no issue, and bulk automated
   or agent-generated changes, are closed without review.
2. Read `CONTEXT.md`. It is the contract; a change that disagrees with it is a defect
   in one of the two, and the pull request must say which.
3. Write the failing test first. `docs/code-review-checklist.md` is what the review
   checks against, and its sharpest question is: *would that test have failed before
   this change?*
4. Run `make test` and say in the pull request that it passed. Compiling is not
   evidence. A change to the boot payload also needs `make vm-smoke`.
5. Keep it to one change. Formatting, renames or refactors unrelated to it go in
   their own pull request.

## What will not be accepted

- Anything that weakens `target_safety` or `sysdisk`: they may be strengthened, never
  relaxed to make something work.
- A real device node, home path, account name, hardware serial or email address in
  code, docs, fixtures or commit messages. Use `/dev/sdX` and `user`;
  `scripts/check-no-identifying-data.sh` runs in CI and will refuse the rest.
- New dependencies without a stated reason, or changes to CI permissions.

Security problems go to the maintainer privately through GitHub's
"Report a vulnerability" on the Security tab, never in a public issue.
