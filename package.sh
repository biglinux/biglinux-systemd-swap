#!/bin/bash -e
################################################################################
# echo wrappers
INFO(){ echo -n "INFO: "; echo "$@" ;}
WARN(){ echo -n "WARN: "; echo "$@" ;}
ERRO(){ echo -n "ERRO: "; echo -n "$@" ; echo " Abort!"; exit 1;}

debian_package(){
    cd "$(dirname $0)"
    VERSION=$(git tag | tail -n 1)
    [ -z "$VERSION" ] && ERRO "Can't get git tag, VERSION are empty!"
    DEB_NAME=systemd-swap_${VERSION}_any
    mkdir -p $DEB_NAME
    make install PREFIX=$DEB_NAME/
    mkdir -p  $DEB_NAME/DEBIAN
    chmod 755 $DEB_NAME/DEBIAN
    {
        echo "Package: systemd-swap"
        echo "Version: $VERSION"
        echo "Section: custom"
        echo "Priority: optional"
        echo "Architecture: all"
        echo "Depends: util-linux"
        echo "Essential: no"
        echo "Installed-Size: 16"
        echo "Maintainer: nefelim4ag@gmail.com"
        echo "Description: Script for creating hybrid swap space from zram swaps, swap files and swap partitions."
    } > $DEB_NAME/DEBIAN/control
    dpkg-deb --build $DEB_NAME
}

archlinux_package(){
    INFO "Use pacman -S systemd-swap"
}

case $1 in
    debian) debian_package ;;
    archlinux) ;;
    *) echo "$0 <debian|archlinux>" ;;
esac
