#!/bin/sh
# The one place this repo decides what a conventional commit subject is.
#
# Two readers, one rule. .githooks/commit-msg runs this on the subject git is
# about to record, and .github/workflows/commits.yml runs it on every subject
# `git log --format=%s` prints for the commits on a pull request. Written twice,
# in two files, the rule is one a contributor's local gate and CI can disagree
# about, and the disagreement is invisible: the commit passes one and fails the
# other, or passes both and lands in a changelog that has no line for it.
#
# Reads subjects on stdin, one per line. Prints ok or REFUSED for each, then
# exits 1 if any was refused. Run with `sh` rather than executed: this is not a
# hook git looks for, and a mode bit would read as though it were.
#
# An empty line is a subject, not a gap in the input. git records one for a
# commit made with --allow-empty-message, release-plz drops it under
# `filter_unconventional = true` in release-plz-changelog.toml, and a gate that
# skipped empty lines would report every subject on that branch conventional.
#
# Types are the conventional-commits set this crate's changelog config parses.
# `revert` and `build` are real conventional types and are refused here on
# purpose: neither has a parser in that config, so either would vanish exactly
# like an unreadable subject does. The scope is `[^()]` because git-conventional
# allows any character in a scope except a parenthesis.

set -eu

bad=0
count=0

# The `|| [ -n ... ]` half keeps a final line with no trailing newline, which
# is what a here-string or a `printf` without a newline hands over.
while IFS= read -r subject || [ -n "$subject" ]; do
    count=$((count + 1))
    if printf '%s' "$subject" | grep -qE '^(feat|fix|perf|refactor|docs|test|ci|chore|style)(\([^()]+\))?!?: .+'; then
        printf '  ok       %s\n' "$subject"
    else
        printf '  REFUSED  %s\n' "${subject:-<empty subject>}"
        bad=$((bad + 1))
    fi
done

if [ "$count" -eq 0 ]; then
    echo "No commit subject to check."
    exit 0
fi

if [ "$bad" -gt 0 ]; then
    echo "$bad of $count commit subject(s) above are not conventional."
    echo "Rewrite them as: type(optional-scope): lowercase summary"
    exit 1
fi

echo "All $count commit subject(s) are conventional."
