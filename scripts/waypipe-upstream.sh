#!/usr/bin/env bash
# Waypane: window-level Wayland forwarding over SSH.
# Copyright (C) 2026  Zhou Qiankang <wszqkzqk@qq.com>
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.
#
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"
prefix=vendor/waypipe
manifest=vendor/waypipe.upstream.toml
export GIT_MERGE_AUTOEDIT=no

value() {
    python3 - "$manifest" "$1" <<'PY'
import sys, tomllib
print(tomllib.load(open(sys.argv[1], "rb"))[sys.argv[2]])
PY
}

repository=$(value repository)
tag=$(value tag)
commit=$(value commit)
latest_tag=$(value latest_tag)
latest_commit=$(value latest_commit)

output() {
    printf '%s=%s\n' "$1" "$2"
    if [ -n "${GITHUB_OUTPUT:-}" ]; then printf '%s=%s\n' "$1" "$2" >> "$GITHUB_OUTPUT"; fi
}

valid_tag() {
    [[ "$1" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]
}

version_gt() {
    python3 - "$1" "$2" <<'PY'
import sys
key = lambda v: tuple(int(p) for p in v.removeprefix("v").split("."))
sys.exit(0 if key(sys.argv[1]) > key(sys.argv[2]) else 1)
PY
}

# Parses `git ls-remote --tags`: annotated tags resolve to their peeled commit.
# With a tag argument prints that tag's commit; without, prints "<latest release> <commit>".
parse_tags() {
    # The program arrives via here-doc on stdin; the pipeline's data is preserved on fd 3.
    python3 - 3<&0 "${1:-}" <<'PY'
import os, re, sys
wanted = sys.argv[1]
tags = {}
for line in os.fdopen(3):
    oid, ref = line.split()
    m = re.fullmatch(r"refs/tags/(v\d+\.\d+\.\d+)(\^\{\})?", ref)
    if not m:
        continue
    name, peeled = m.groups()
    if peeled or name not in tags:
        tags[name] = oid
if wanted:
    commit = tags.get(wanted)
    if not commit:
        sys.exit(f"upstream release tag {wanted} not found")
else:
    if not tags:
        sys.exit("no upstream release tags returned")
    wanted = max(tags, key=lambda v: tuple(map(int, v[1:].split("."))))
    commit = f"{wanted} {tags[wanted]}"
print(commit)
PY
}

remote_tags() {
    git ls-remote --tags "$repository" 'v*'
}

resolve_tag() {
    remote_tags | parse_tags "$1"
}

# Fetches one upstream tag into a private namespace with --no-tags, so upstream v* tags
# never enter refs/tags and cannot become Waypane's APK version via git describe.
fetch_tag() {
    local name=$1 expected=$2 peeled
    git fetch -q --no-tags "$repository" "+refs/tags/$name:refs/waypipe-upstream/$name"
    peeled=$(git rev-parse "refs/waypipe-upstream/${name}^{commit}")
    if [ "$peeled" != "$expected" ]; then
        echo "upstream tag $name resolved to $peeled, expected $expected" >&2
        return 1
    fi
    printf '%s\n' "$peeled"
}

archive_commit() {
    local oid=$1 dest=$2
    mkdir -p "$dest"
    git archive "$oid" | tar -x -C "$dest"
}

tree_of_files() {
    # Stage in a throwaway repository so the archive's own .gitignore cannot skip files.
    (
        cd "$1"
        git init -q
        git add --all --force
        tree=$(git write-tree)
        rm -rf .git
        printf '%s\n' "$tree"
    )
}

write_manifest() {
    cat > "$manifest" <<EOF
# Provenance for the read-only upstream Waypipe copy in vendor/waypipe.
# Managed by scripts/waypipe-upstream.sh. commit is the peeled tag commit.
repository = "$repository"
tag = "$tag"
commit = "$commit"
latest_tag = "$latest_tag"
latest_commit = "$latest_commit"
EOF
}

record_tags() {
    git tag --list > target/waypipe-tags-before.tmp
}

assert_tags_unchanged() {
    git tag --list > target/waypipe-tags-after.tmp
    if ! cmp -s target/waypipe-tags-before.tmp target/waypipe-tags-after.tmp; then
        echo "Waypane tags changed while importing upstream: restore refs/tags before continuing" >&2
        diff target/waypipe-tags-before.tmp target/waypipe-tags-after.tmp || true
        exit 1
    fi
    rm -f target/waypipe-tags-before.tmp target/waypipe-tags-after.tmp
}

case "${1:-}" in
    check)
        found=$(remote_tags | parse_tags)
        read -r newest newest_commit <<< "$found"
        available=false
        if version_gt "$newest" "$latest_tag"; then available=true; fi
        output current_tag "$tag"
        output latest_tag "$newest"
        output latest_commit "$newest_commit"
        output update_available "$available"
        ;;
    verify)
        git diff --quiet HEAD -- "$prefix" || { echo "unstaged changes in $prefix" >&2; exit 1; }
        git diff --cached --quiet HEAD -- "$prefix" || { echo "staged changes in $prefix" >&2; exit 1; }
        if [ -n "$(git ls-files --others --exclude-standard -- "$prefix")" ]; then
            echo "untracked files in $prefix" >&2; exit 1
        fi
        oid=$(fetch_tag "$tag" "$commit")
        tmp=$(mktemp -d)
        trap 'rm -rf "$tmp"' EXIT
        archive_commit "$oid" "$tmp/upstream"
        upstream_tree=$(tree_of_files "$tmp/upstream")
        vendor_tree=$(git rev-parse "HEAD:$prefix")
        if [ "$vendor_tree" != "$upstream_tree" ]; then
            echo "$prefix differs from upstream $tag ($oid)" >&2
            echo "vendor tree $vendor_tree, upstream tree $upstream_tree" >&2
            git diff --no-index --stat "$tmp/upstream" "$prefix" >&2 || true
            exit 1
        fi
        echo "$prefix matches upstream $tag ($oid)"
        ;;
    export)
        [ $# = 4 ] && valid_tag "$2" && [[ "$3" =~ ^[0-9a-f]{40}$ ]] ||
            { echo "usage: $0 export TAG COMMIT DIRECTORY (empty or missing)" >&2; exit 2; }
        [ ! -e "$3" ] || [ -z "$(ls -A "$3")" ] || { echo "$3 is not empty" >&2; exit 1; }
        oid=$(fetch_tag "$2" "$3")
        archive_commit "$oid" "$4"
        echo "exported upstream $2 ($oid) to $4"
        ;;
    update)
        [ $# = 2 ] && valid_tag "$2" || { echo "usage: $0 update TAG" >&2; exit 2; }
        target=$2
        if ! version_gt "$target" "$tag"; then
            if [ "$target" = "$tag" ]; then
                echo "$prefix is already at $tag ($commit)"
                exit 0
            fi
            echo "refusing to downgrade $prefix from $tag to $target" >&2
            exit 1
        fi
        git diff --quiet && git diff --cached --quiet || { echo "working tree must be clean before update" >&2; exit 1; }
        target_commit=$(resolve_tag "$target")
        if [ "$target_commit" = "$commit" ]; then
            echo "$target resolves to the vendored commit $commit; nothing to do"
            exit 0
        fi
        mkdir -p target
        record_tags
        before=$(git describe --tags --abbrev=12 --always HEAD)
        oid=$(fetch_tag "$target" "$target_commit")
        git subtree pull --prefix="$prefix" --squash -m "vendor(waypipe): import upstream $target" . "$oid"
        assert_tags_unchanged
        after=$(git describe --tags --abbrev=12 --always HEAD)
        tag=$target
        commit=$target_commit
        if version_gt "$target" "$latest_tag"; then latest_tag=$target; latest_commit=$target_commit; fi
        write_manifest
        git add "$manifest"
        echo "imported $prefix from upstream $tag ($commit)"
        echo "Waypane version description on this checkout: before=$before, after=$after"
        echo "Review the changes and protocol XML copies, then commit $manifest."
        ;;
    *)
        echo "usage: $0 {check|verify|update TAG|export TAG COMMIT DIRECTORY}" >&2
        exit 2
        ;;
esac
