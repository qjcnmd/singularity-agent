# Issue tracker: GitHub

Issues and PRDs for this repo live as GitHub issues. Use the `gh` CLI for all operations.

## Conventions

- Create: `gh issue create --title "..." --body "..."`
- Read: `gh issue view <number> --comments`
- List: `gh issue list --state open`
- Comment: `gh issue comment <number> --body "..."`
- Apply/remove labels: `gh issue edit <number> --add-label "..."` / `--remove-label "..."`
- Close: `gh issue close <number> --comment "..."`

Infer the repository from `git remote -v`; when run inside this clone, `gh` does this automatically.

## Pull requests as a triage surface

**PRs as a request surface: no.**

## Skill routing

When a skill says “publish to the issue tracker”, create a GitHub issue.
When it says “fetch the relevant ticket”, run `gh issue view <number> --comments`.
