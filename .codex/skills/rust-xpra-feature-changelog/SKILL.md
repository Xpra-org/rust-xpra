---
name: rust-xpra-feature-changelog
description: Generate or update rust-xpra CHANGELOG.md release entries from Git history. Use when asked to list features added since a release, include changes from intermediate patch releases, classify entries under the existing platform/network/encoding/feature headings, link entries to commits, and exclude fixes, refactoring, cleanup, and log-only changes.
---

# Rust Xpra Feature Changelog

Generate concise, feature-only release notes from commits after a requested base release.

## Workflow

1. Read `CHANGELOG.md`, inspect `git status --short`, and preserve unrelated worktree changes.
2. Resolve the requested release to the actual tag. Account for version/tag differences such as release `0.2.0` being tagged `v0.2`.
3. Use every commit reachable from the base tag through `HEAD`. If an intermediate release such as `0.2.1` has no tag, do not omit its changes; the full `BASE..HEAD` range already includes them.
4. Inspect commit subjects, bodies, stats, and relevant diffs. Use README or capability changes as supporting evidence. Do not classify from subjects alone.
5. Include commits that add a capability users, servers, packagers, or supported environments can use, including:
   - protocol, authentication, compression, or remote-integration support;
   - image/video encoding support;
   - window, cursor, notification, bell, desktop, or other client behavior;
   - new platform, build, linking, or packaging options.
6. Exclude:
   - bug fixes and corrections to existing behavior;
   - graceful failure or error-handling changes;
   - resource cleanup and decoder lifecycle fixes;
   - refactoring, file moves, helper extraction, and code cleanup;
   - warning-level, formatting, diagnostic, or debug-log-only changes.
7. Preserve the current release heading, category names, emoji, indentation, and wording style. Do not change the release version or date unless requested.
8. Write each entry as a short imperative capability description with a full commit link:

   ```markdown
   * 🖧 Network:
     * [send `ping` packets](https://github.com/Xpra-org/rust-xpra/commit/FULL_HASH)
   ```

9. Put entries under the most specific existing category:
   - `🔧 Platforms, build and packaging`
   - `🖧 Network`
   - `🌈 Encodings`
   - `✨ Features`
10. Reuse a commit in more than one category only when it delivers genuinely distinct facets, such as an encoding plus a packaging/linking option.
11. Leave excluded commits out of `CHANGELOG.md`; do not add a fixes or refactoring section.

## Validation

1. Compare the final entries with the complete `BASE..HEAD` commit list and confirm every included commit is a feature.
2. Verify every linked full hash resolves to a commit.
3. Run:

   ```bash
   git diff --check -- CHANGELOG.md
   git diff -- CHANGELOG.md
   ```

4. Confirm only `CHANGELOG.md` and intentional skill files changed during this task.
