#!/bin/sh
# Build an RPM of the commit checked out in this repository, not of
# uncommitted changes (doc 14 §RPM package). Output: target/rpm/RPMS/.
set -eu

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
commit=$(git -C "$root" rev-parse HEAD)
version=$(git -C "$root" show "$commit:Cargo.toml" | sed -n 's/^version = "\(.*\)"/\1/p' | head -n1)
top="$root/target/rpm"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git clone --quiet --no-checkout "$root" "$work/wayterm-$version"
git -C "$work/wayterm-$version" checkout --quiet --detach "$commit"

mkdir -p "$top/SOURCES"
tar -czf "$top/SOURCES/wayterm-$version.tar.gz" -C "$work" "wayterm-$version"
rpmbuild -bb \
  --define "_topdir $top" \
  --define "wayterm_version $version" \
  "$root/packaging/rpm/wayterm.spec"
