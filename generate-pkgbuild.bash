#!/usr/bin/env bash
# Generate the AUR PKGBUILD for a source build of hegn.
#
# Usage: generate-pkgbuild.bash <tag>
# The generated PKGBUILD is written next to this script.
set -euo pipefail

cd "$(dirname "$0")"
source ./aur-pkgbuild-common.bash

tag="${1:-}"
pkgver=$(aur_tag_pkgver "$tag")
url=$(aur_repo_url)

cat >PKGBUILD <<EOF
# Maintainer: hegn maintainers
pkgname=hegn
pkgver=${pkgver}
pkgrel=1
pkgdesc='${aur_description}'
arch=('x86_64' 'aarch64')
url='${url}'
license=('MIT' 'Apache-2.0')
# cloud-hypervisor-bin provides 'cloud-hypervisor', so either AUR package
# satisfies this dependency.
depends=('gcc-libs' 'cloud-hypervisor')
conflicts=('hegn-bin' 'hegn-git')
makedepends=('git' 'rust')
source=("\$pkgname::git+${url}.git#tag=${tag}")
sha256sums=('SKIP')

build() {
    cd "\$srcdir/\$pkgname"
    export CARGO_TARGET_DIR="\$srcdir/target"
    cargo build --release --locked
}

package() {
    cd "\$srcdir/\$pkgname"
    install -Dm755 "\$srcdir/target/release/hegn" "\$pkgdir/usr/bin/hegn"
}
EOF

echo "wrote PKGBUILD for hegn ${pkgver} (tag ${tag})"
