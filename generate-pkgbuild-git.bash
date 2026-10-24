#!/usr/bin/env bash
# Generate the AUR PKGBUILD for the hegn-git VCS package.
#
# Usage: generate-pkgbuild-git.bash
# The generated PKGBUILD is written next to this script. hegn.desktop and
# hegn.install must exist there as well; the publish workflow uploads them as
# AUR assets.
set -euo pipefail

cd "$(dirname "$0")"
source ./aur-pkgbuild-common.bash

for file in Cargo.toml hegn.desktop hegn.install; do
  if [[ ! -f "$file" ]]; then
    echo "missing $file" >&2
    exit 1
  fi
done

url=$(aur_repo_url)
version=$(aur_cargo_version Cargo.toml)
pkgver=$(aur_git_pkgver "$version")
desktop_sha256=$(aur_file_sha256 hegn.desktop)

cat >PKGBUILD <<EOF
# Maintainer: Henrik Bernhardt <57109108+Henriklmao@users.noreply.github.com>
pkgname=hegn-git
pkgver=${pkgver}
pkgrel=1
pkgdesc='${aur_description} (git version)'
arch=('x86_64' 'aarch64')
url='${url}'
license=('MIT' 'Apache-2.0')
# cloud-hypervisor-bin provides 'cloud-hypervisor', so either AUR package
# satisfies this dependency. hegn cannot start without the VMM or virtiofsd.
depends=('gcc-libs' 'git' 'cloud-hypervisor' 'virtiofsd')
optdepends=(
  'lazygit: open projects with the g shortcut'
)
provides=('hegn')
conflicts=('hegn' 'hegn-bin')
makedepends=('cargo' 'rust' 'git')
install=hegn.install
# aws-lc-sys (pulled in by rustls/reqwest) does not link with makepkg's
# GCC LTO flags; the Rust release profile already uses thin LTO.
# The release profile strips symbols, so a -debug package would be empty.
options=('!lto' '!debug')
source=('hegn::git+${url}.git'
  'hegn.desktop')
sha256sums=('SKIP'
  '${desktop_sha256}')

pkgver() {
    cd "\$srcdir/hegn"
    local version
    version=\$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
    printf '%s.r%s.g%s' "\$version" "\$(git rev-list --count HEAD)" "\$(git rev-parse --short HEAD)"
}

build() {
    cd "\$srcdir/hegn"
    export CARGO_TARGET_DIR="\$srcdir/target"
    cargo build --release --locked
}

package() {
    cd "\$srcdir/hegn"
    install -Dm755 "\$srcdir/target/release/hegn" "\$pkgdir/usr/bin/hegn"
    install -Dm644 "\$srcdir/hegn.desktop" "\$pkgdir/usr/share/applications/hegn.desktop"
}
EOF

echo "wrote PKGBUILD for hegn-git ${pkgver} (${url})"
