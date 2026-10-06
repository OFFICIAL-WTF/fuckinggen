#!/usr/bin/env bash
# Publish (or update) the AUR package that ships the fgen binaries.
#
#   ./publish.sh              # publish the version in Cargo.toml
#   ./publish.sh 0.2.0        # or an explicit version
#
# This regenerates PKGBUILD and .SRCINFO from the tagged upstream tarball so
# the two can never drift, then pushes them to ssh://aur@aur.archlinux.org.
# Needs curl, git, shasum (or sha256sum) and an SSH key registered with the AUR
# account (~/.ssh/aur, wired up in ~/.ssh/config).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

pkgname=fgen
repo="prophesourvolodymyr/fuckinggen"
upstream="https://github.com/$repo"
auri="ssh://aur@aur.archlinux.org/$pkgname.git"

version="${1:-$(sed -n 's/^version = "\(.*\)"$/\1/p' ../../Cargo.toml | head -1)}"
[ -n "$version" ] || { echo "could not determine the version" >&2; exit 1; }
tag="v$version"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "==> fetching $upstream/archive/refs/tags/$tag.tar.gz"
curl -L --fail --silent --show-error -o "$work/src.tar.gz" "$upstream/archive/refs/tags/$tag.tar.gz"
if command -v shasum >/dev/null 2>&1; then
  sha="$(shasum -a 256 "$work/src.tar.gz" | cut -d' ' -f1)"
else
  sha="$(sha256sum "$work/src.tar.gz" | cut -d' ' -f1)"
fi
echo "    sha256 $sha"

source_url="$pkgname-$version.tar.gz::$upstream/archive/refs/tags/$tag.tar.gz"
desc='Generate images with your ChatGPT subscription from the terminal'

cat > PKGBUILD <<PKGBUILD_EOF
pkgname=$pkgname
pkgver=$version
pkgrel=1
pkgdesc='$desc'
arch=('x86_64' 'aarch64')
url='$upstream'
license=('WTFPL')
depends=('glibc')
makedepends=('rust' 'gcc')
source=("$source_url")
sha256sums=('$sha')

build() {
  cd "\$srcdir/fuckinggen-\$pkgver"
  cargo build --release --locked
}

package() {
  cd "\$srcdir/fuckinggen-\$pkgver"

  install -Dm755 target/release/fgen "\$pkgdir/usr/bin/fgen"
  install -Dm755 target/release/fuckinggen "\$pkgdir/usr/bin/fuckinggen"
  install -Dm644 skills/gpt-image-gen-latest/SKILL.md \
    "\$pkgdir/usr/share/fgen/skills/gpt-image-gen-latest/SKILL.md"
  install -Dm755 install.sh "\$pkgdir/usr/share/fgen/install.sh"
  install -Dm644 README.md "\$pkgdir/usr/share/doc/fgen/README.md"
  install -Dm644 LICENSE "\$pkgdir/usr/share/licenses/fgen/LICENSE"
}
PKGBUILD_EOF

{
  printf 'pkgbase = %s\n' "$pkgname"
  printf '\tpkgdesc = %s\n' "$desc"
  printf '\tpkgver = %s\n' "$version"
  printf '\tpkgrel = 1\n'
  printf '\turl = %s\n' "$upstream"
  printf '\tarch = x86_64\n'
  printf '\tarch = aarch64\n'
  printf '\tlicense = WTFPL\n'
  printf '\tdepends = glibc\n'
  printf '\tmakedepends = rust\n'
  printf '\tmakedepends = gcc\n'
  printf '\tsource = %s\n' "$source_url"
  printf '\tsha256sums = %s\n' "$sha"
  printf '\n'
  printf 'pkgname = %s\n' "$pkgname"
} > .SRCINFO

echo "==> syncing to $auri"
if ! git clone --quiet "$auri" "$work/aur" 2>/dev/null; then
  mkdir -p "$work/aur"
  git -C "$work/aur" init --quiet
  git -C "$work/aur" remote add origin "$auri"
fi
cp PKGBUILD .SRCINFO "$work/aur/"
git -C "$work/aur" add PKGBUILD .SRCINFO
if git -C "$work/aur" diff --cached --quiet; then
  echo "==> AUR already up to date: $pkgname $version-1"
  exit 0
fi
name="$(git -C "$work/aur" config user.name || git config --global user.name || echo ProfessorVVS)"
email="$(git -C "$work/aur" config user.email || git config --global user.email || echo professorvvs@users.noreply.github.com)"
if git -C "$work/aur" rev-parse --verify HEAD >/dev/null 2>&1; then
  message="upgpkg: $pkgname $version-1"
else
  message="$pkgname $version-1"
fi
git -C "$work/aur" -c user.name="$name" -c user.email="$email" commit --quiet -m "$message"
# AUR repositories live on master; `git init` may have started us on main.
git -C "$work/aur" branch -M master
git -C "$work/aur" push --quiet origin master
echo "==> published: https://aur.archlinux.org/packages/$pkgname"
