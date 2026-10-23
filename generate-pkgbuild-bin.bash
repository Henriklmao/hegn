#!/usr/bin/env bash
# Generate the AUR PKGBUILD for the pre-built hegn release binary.
#
# Usage: generate-pkgbuild-bin.bash <tag>
# The generated PKGBUILD is written next to this script.
set -euo pipefail

cd "$(dirname "$0")"
source ./aur-pkgbuild-common.bash

tag="${1:-}"
pkgver=$(aur_tag_pkgver "$tag")
url=$(aur_repo_url)
# This is the asset name produced by .github/workflows/rust.yml.
asset="hegn-${tag}-x86_64-linux-gnu"

cat >PKGBUILD <<EOF
# Maintainer: hegn maintainers
pkgname=hegn-bin
pkgver=${pkgver}
pkgrel=1
pkgdesc='${aur_description} (pre-built binary)'
arch=('x86_64')
url='${url}'
license=('MIT' 'Apache-2.0')
depends=('gcc-libs')
provides=('hegn')
conflicts=('hegn' 'hegn-git')
source=("\$pkgname-\$pkgver::${url}/releases/download/${tag}/${asset}")
sha256sums=('SKIP')

package() {
    install -Dm755 "\$srcdir/\$pkgname-\$pkgver" "\$pkgdir/usr/bin/hegn"
}
EOF

echo "wrote PKGBUILD for hegn-bin ${pkgver} (asset ${asset})"
