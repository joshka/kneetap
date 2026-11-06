# Agent guide

kneetap is a Cargo workspace under `crates/`. Run `just ci` before finishing a change.

Read the convention that applies to the task:

- [Code](docs/conventions/code.md): layout, ordering, and imports.
- [Docs](docs/conventions/docs.md): what comments and rustdoc should say.
- [Testing](docs/conventions/testing.md): where tests go and how they read.
- [Commits](docs/conventions/commits.md): how changes are shaped and described.

## Merging

- Squash-merge PRs, so `main` gets one commit per PR.
- Write the squash message as a commit in its own right, following the
  [commit conventions](docs/conventions/commits.md), and end its subject with the PR number, as in
  `Add the kneetap server (#1)`. Don't paste the PR title and description.
- GitHub refuses `gh pr merge` and the plain merge endpoint for a PR in a stack. Use
  `PUT /repos/{owner}/{repo}/pulls/{number}/merge-async` with `merge_method=squash`, `sha`,
  `commit_title`, and `commit_message`.
- That endpoint also merges every open PR below the target, so merge a stack from the bottom up.
- A squash leaves the next PR's parent off `main`, so rebase the rest of the stack onto the new
  commit before merging it.
