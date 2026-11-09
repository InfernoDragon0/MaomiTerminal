#!/usr/bin/env bash
# Regenerates the parser fixtures in this directory from real git output.
# Run from Git Bash (or any POSIX shell): bash crates/git-core/tests/fixtures/make-fixtures.sh
# Deterministic: fixed identities, dates and config, global/system config ignored.
# The args below must match the parser modules; tests/parsers.rs checks the args-*.txt files.
set -euo pipefail

OUT="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/.config"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$WORK/home/.gitconfig"
export LC_ALL=C LANG=C GIT_PAGER=cat GIT_EDITOR=true
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
mkdir -p "$HOME"
: >"$GIT_CONFIG_GLOBAL"
gc() { git config --global "$@"; }
gc init.defaultBranch main
gc core.autocrlf false
gc core.safecrlf false
gc core.protectNTFS false
gc commit.gpgsign false
gc tag.gpgsign false
gc advice.detachedHead false
gc protocol.file.allow always
gc merge.conflictStyle merge
gc user.name "Ada Lovelace"
gc user.email "ada@example.com"

export GIT_AUTHOR_NAME="Ada Lovelace" GIT_AUTHOR_EMAIL="ada@example.com"
export GIT_COMMITTER_NAME="Grace Hopper" GIT_COMMITTER_EMAIL="grace@example.com"
T=1696766400
tick() {
    T=$((T + 3600))
    export GIT_AUTHOR_DATE="$T +0200" GIT_COMMITTER_DATE="$T +0200"
}
tick

newrepo() {
    git init -q "$@"
    local dir="${!#}"
    git -C "$dir" config core.filemode false
    git -C "$dir" config core.symlinks false
    git -C "$dir" config core.ignorecase false
}

STATUS_ARGS=(--porcelain=v2 -z --branch --show-stash --untracked-files=all --ignore-submodules=none)
LOG_FORMAT='%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%cn%x1f%ce%x1f%cd%x1f%D%x1f%s%x1f%b'
LOG_ARGS=(-z "--format=$LOG_FORMAT" --date=raw --decorate=full --no-color --no-show-signature)
DIFF_ARGS=(--no-color --no-ext-diff --no-textconv -p -U3 --find-renames --submodule=short --src-prefix=a/ --dst-prefix=b/)
REFS_FORMAT='%(refname)%1f%(refname:short)%1f%(objectname)%1f%(*objectname)%1f%(HEAD)%1f%(upstream)%1f%(upstream:track,nobracket)%1f%(creatordate:unix)%1f%(contents:subject)'
REFS_ARGS=("--format=$REFS_FORMAT" --sort=refname refs/heads refs/remotes refs/tags refs/stash)
STASH_ARGS=(list -z '--format=%gd%x1f%H%x1f%ct%x1f%gs')

printf '%s\n' "${LOG_ARGS[@]}" >"$OUT/args-log.txt"
printf '%s\n' "${REFS_ARGS[@]}" >"$OUT/args-refs.txt"
printf '%s\n' "${STASH_ARGS[@]}" >"$OUT/args-stash.txt"
printf '%s\n' "${DIFF_ARGS[@]}" >"$OUT/args-diff.txt"
printf '%s\n' "${STATUS_ARGS[@]}" >"$OUT/args-status.txt"

lines() { seq -f "${3:-line} %g" "$1" "$2"; }

# ---- history repo: log, refs, stashes, remotes, tracking status -------------------------
cd "$WORK"
git init -q --bare origin.git
git init -q --bare upstream.git
git init -q --bare upstream-push.git
newrepo hist
cd hist
git remote add origin ../origin.git
git remote add upstream ../upstream.git
git remote set-url --push upstream ../upstream-push.git

lines 1 20 >README.md
git add README.md
git commit -qm "Root commit"
git tag v0.1
tick

lines 1 21 >README.md
printf 'Add feature flag\n\nThis body has two paragraphs.\nSecond line, unicode: h\xc3\xa9llo \xe2\x9c\x93\n\n    indented code\n' >msg
git commit -qa -F msg
tick

git checkout -qb feature
printf 'feature\n' >feature.txt
git add feature.txt
GIT_AUTHOR_NAME="$(printf 'Zo\xc3\xab Z\xc3\xbcrich')" GIT_COMMITTER_DATE="$T -0530" git commit -qm "Feature work"
tick
git checkout -q main
lines 2 21 >README.md
git commit -qam "Drop first line"
tick
git merge -q --no-ff -m "Merge branch 'feature'" feature
tick
git tag -a v1.0 -m "Release 1.0" -m "Tag body"
tick

git push -q -u origin main feature
git push -q upstream main
git remote set-head origin main

printf 'Multi line\nsubject paragraph\n\nAnd a body.\n' >msg
git commit -q --allow-empty -F msg
tick
git commit -q --allow-empty --allow-empty-message -m ""
tick

# origin/main gets two commits we do not have: ahead 2, behind 2
git clone -q ../origin.git "$WORK/other"
(
    cd "$WORK/other"
    git config core.filemode false
    git commit -q --allow-empty -m "Remote one"
    tick
    git commit -q --allow-empty -m "Remote two"
    git push -q origin main
)
tick
git fetch -q origin

git checkout -qb gone-branch
git commit -q --allow-empty -m "Soon gone"
tick
git push -q -u origin gone-branch
git push -q origin --delete gone-branch
git checkout -q main

printf 'stash me\n' >>README.md
git stash push -q -m "first stash"
tick
printf 'stash me too\n' >>README.md
git stash push -q
tick
rm -f msg

git log "${LOG_ARGS[@]}" --all >"$OUT/log-all.bin"
git for-each-ref "${REFS_ARGS[@]}" >"$OUT/refs.bin"
git stash "${STASH_ARGS[@]}" >"$OUT/stashes.bin"
git remote -v >"$OUT/remotes.bin"
git rev-list --left-right --count main...origin/main >"$OUT/ahead-behind.bin"
git status "${STATUS_ARGS[@]}" >"$OUT/status-tracking.bin"
git checkout -q gone-branch
git status "${STATUS_ARGS[@]}" >"$OUT/status-gone.bin"
git checkout -q --detach v1.0
git status "${STATUS_ARGS[@]}" >"$OUT/status-detached.bin"
git log "${LOG_ARGS[@]}" -n 3 >"$OUT/log-detached.bin"
git show --format= "${DIFF_ARGS[@]}" HEAD~1 >"$OUT/diff-show.bin"
git checkout -q main

# ---- initial repo: no commits yet --------------------------------------------------------
cd "$WORK"
newrepo initial
cd initial
printf 'staged\n' >staged.txt
git add staged.txt
printf 'untracked\n' >untracked.txt
git status "${STATUS_ARGS[@]}" >"$OUT/status-initial.bin"

# ---- work repo: every status entry kind, submodule ----------------------------------------
cd "$WORK"
newrepo sub
(
    cd sub
    printf 'sub\n' >s.txt
    git add s.txt
    git commit -qm "Sub root"
)
tick
newrepo work
cd work
lines 1 30 mod >modified.txt
lines 1 30 both >both.txt
lines 1 30 del >deleted.txt
lines 1 40 ren >renamed-src.txt
lines 1 40 copy >copy-src.txt
lines 1 5 type >type.txt
lines 1 5 spaced >"name with spaces.txt"
lines 1 5 uni >"$(printf '\xc3\xbcn\xc3\xafc\xc3\xb6d\xc3\xa9.txt')"
printf '*.log\nbuild/\n' >.gitignore
git add .
git submodule add -q ../sub sub
git commit -qm "Base"
tick

printf 'changed\n' >>modified.txt
printf 'staged\n' >>both.txt
git add both.txt
printf 'unstaged\n' >>both.txt
git rm -q deleted.txt
git mv renamed-src.txt renamed-dst.txt
cp copy-src.txt copy-dst.txt
printf 'tweak\n' >>copy-src.txt
git add copy-src.txt copy-dst.txt
printf 'added\n' >added.txt
git add added.txt
printf 'gone\n' >added-then-deleted.txt
git add added-then-deleted.txt
rm added-then-deleted.txt
link=$(printf 'target' | git hash-object -w --stdin)
git update-index --cacheinfo "120000,$link,type.txt"
printf 'more\n' >>"name with spaces.txt"
printf 'more\n' >>"$(printf '\xc3\xbcn\xc3\xafc\xc3\xb6d\xc3\xa9.txt')"
git add "$(printf '\xc3\xbcn\xc3\xafc\xc3\xb6d\xc3\xa9.txt')"
mkdir -p "untracked dir" build
printf 'u\n' >"untracked dir/new file.txt"
printf 'u\n' >"$(printf 'caf\xc3\xa9.md')"
printf 'log\n' >debug.log
printf 'obj\n' >build/out.o
printf 'dirty\n' >>sub/s.txt
git -c status.renames=copies status "${STATUS_ARGS[@]}" >"$OUT/status-work.bin"
git status "${STATUS_ARGS[@]}" --ignored >"$OUT/status-ignored.bin"
git diff "${DIFF_ARGS[@]}" -- sub >"$OUT/diff-submodule.bin"

# ---- diff repo -----------------------------------------------------------------------------
cd "$WORK"
newrepo diffs
cd diffs
lines 1 40 >multi.txt
printf 'one\r\ntwo\r\nthree\r\n' >crlf.txt
printf 'no newline' >noeol.txt
printf 'has newline\n' >gets-noeol.txt
printf 'bin\000ary\001\002' >binary.bin
lines 1 10 del >delete-me.txt
: >empty-delete.txt
lines 1 30 ren >rename-src.txt
lines 1 30 pure >pure-rename-src.txt
lines 1 30 copy >copy-src.txt
lines 1 5 mode >mode.sh
lines 1 5 spaced >"name with spaces.txt"
lines 1 5 uni >"$(printf '\xc3\xbcn\xc3\xafc\xc3\xb6d\xc3\xa9.txt')"
git add .
git commit -qm "Diff base"
tick

# unstaged changes
sed -e 's/^line 3$/line three/' -e 's/^line 35$/line thirty-five/' multi.txt >tmp && mv tmp multi.txt
printf 'one\r\nTWO\r\nthree\r\n' >crlf.txt
printf 'no newline\nnow two lines' >noeol.txt
printf 'has newline\nnot any more' >gets-noeol.txt
printf 'bin\000ary\001\003' >binary.bin
printf 'line 6\n' >>"name with spaces.txt"
printf 'line 6\n' >>"$(printf '\xc3\xbcn\xc3\xafc\xc3\xb6d\xc3\xa9.txt')"
git diff "${DIFF_ARGS[@]}" >"$OUT/diff-worktree.bin"
git checkout -q -- .

# staged changes
printf 'a\nb\nc\n' >added.txt
: >empty-added.txt
printf 'new\000binary' >added.bin
git rm -q delete-me.txt empty-delete.txt
git mv rename-src.txt renamed.txt
sed -e 's/^ren 15$/ren fifteen/' renamed.txt >tmp && mv tmp renamed.txt
git mv pure-rename-src.txt "pure renamed.txt"
cp copy-src.txt copy-dst.txt
git update-index --chmod=+x mode.sh
git add .
git diff --cached "${DIFF_ARGS[@]}" >"$OUT/diff-cached.bin"
git diff --cached "${DIFF_ARGS[@]}" --find-copies-harder >"$OUT/diff-copies.bin"
git diff --cached "${DIFF_ARGS[@]}" --no-renames -- empty-added.txt empty-delete.txt >"$OUT/diff-empty.bin"
git commit -qm "Staged changes"
tick

# names git must C-quote; not representable on NTFS, so they only live in the index
base=$(git rev-parse HEAD)
blob=$(lines 1 3 | git hash-object -w --stdin)
for name in "$(printf 'tab\there')" 'quote"d' 'back\slash' "$(printf 'new\nline')" \
    "$(printf 'caf\xc3\xa9 \xe2\x9c\x93.txt')"; do
    git update-index --add --cacheinfo "100644,$blob,$name"
done
spaced=$(git rev-parse ":name with spaces.txt")
git rm -q --cached "name with spaces.txt"
git update-index --add --cacheinfo "100644,$spaced,$(printf 'renamed\ttab.txt')"
git commit -qm "Odd names"
tick
git diff "${DIFF_ARGS[@]}" "$base" HEAD >"$OUT/diff-quoted.bin"
git -c core.quotepath=off diff "${DIFF_ARGS[@]}" "$base" HEAD >"$OUT/diff-quoted-off.bin"

# ---- conflict repo ---------------------------------------------------------------------------
cd "$WORK"
newrepo conflict
cd conflict
printf 'base\n' >uu.txt
printf 'base\n' >du.txt
printf 'base\n' >ud.txt
git add .
git commit -qm "Conflict base"
tick
git checkout -qb other
printf 'theirs\n' >uu.txt
printf 'theirs\n' >du.txt
git rm -q ud.txt
printf 'theirs\n' >aa.txt
git add .
git commit -qm "Theirs"
tick
git checkout -q main
printf 'ours\n' >uu.txt
git rm -q du.txt
printf 'ours\n' >ud.txt
printf 'ours\n' >aa.txt
git add .
git commit -qm "Ours"
tick
git merge -q other >/dev/null 2>&1 || true
git status "${STATUS_ARGS[@]}" >"$OUT/status-conflict.bin"
git diff "${DIFF_ARGS[@]}" >"$OUT/diff-conflict.bin"

echo "fixtures written to $OUT"
