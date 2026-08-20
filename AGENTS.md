# Entangled Desktop

Read `CLAUDE.md` for the repository architecture, build workflow, and hard
rules.

Project-specific Codex skills live under `.agents/skills/`. Load the matching
skill before working on its subsystem, and always load `dev-environment` before
the first build, worktree creation, demo VM launch, or long test campaign.

Every project skill edit must update its twin in the same change: the Codex
copy under `.agents/skills/` and the Claude copy under `.claude/skills/`. Keep
both copies materially equivalent and validate both before the change is
complete.
