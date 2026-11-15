# CLAUDE.md
@README.md

Read [README.md](README.md) immediately. It is the plan and source of truth for this project.

Tasks and open questions: [todo.txt](todo.txt). Locked decisions are final.

## Git

- Commit after every small change, with a clear message. Never batch changes.
- **HARD RULE: push to GitHub (`origin`) after every commit.** Never leave commits only local. Parallel subagents work from what is on GitHub. Pull with rebase before pushing; if a push is rejected, rebase and push again.
- Never leave branches open long. Keep them short-lived: merge to `main` quickly, then delete the branch.
