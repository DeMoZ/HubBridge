## 1. Remove the agent notes

- [x] 1.1 `git rm CLAUDE.md`; verify `git grep -n CLAUDE.md -- ':!openspec/changes/archive' ':!openspec/changes/move-agent-notes'` finds nothing
- [x] 1.2 Verify `openspec/config.yaml` still has its `context:` block
