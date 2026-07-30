#!/bin/bash
# Entry point for https://github.com/Xpra-org/repo-build-scripts `build_debs.sh`, which
# runs it from this directory. xpra's own packaging splits this into a per-package script
# because it builds four of them; there is only `rust-xpra` here, so it is all one file.

eval `dpkg-architecture -s`

if [ -z "${REPO_ARCH_PATH}" ]; then
	REPO_ARCH_PATH="`pwd`/../repo"
fi

#find the latest version we can build
#(`download_source.sh` put it there, from the `Source:` URL in rust-xpra.spec):
RUST_XPRA_TAR=`ls ../pkgs/rust-xpra-0.3.*.tar.gz 2> /dev/null | sort -V | tail -n 1`
if [ -z "${RUST_XPRA_TAR}" ]; then
	echo "no rust-xpra source found"
	exit 0
fi

dirname=`echo ${RUST_XPRA_TAR} | sed 's+../pkgs/++g' | sed 's/.tar.gz//'`
rm -fr "./${dirname}"
tar -zxf ${RUST_XPRA_TAR}
pushd "./${dirname}"
ln -sf packaging/debian/rust-xpra ./debian

#install build dependencies:
mk-build-deps --install --tool='apt-get -o Debug::pkgProblemResolver=yes --no-install-recommends --yes' debian/control
rm -f rust-xpra-build-deps*

#skip the build if this package is already in the repository:
CHANGELOG_VERSION=`head -n 1 "./debian/changelog" | sed 's/.*(//g' | sed 's/).*//g'`
DEB_FILENAME="rust-xpra_${CHANGELOG_VERSION}_$DEB_BUILD_ARCH.deb"
if [ `find $REPO_ARCH_PATH/ -name "${DEB_FILENAME}" | wc -l` != "0" ]; then
	echo "package already exists"
else
	if [ `arch` == "aarch64" ]; then
		debuild --no-lintian -us -uc -b
	else
		debuild -us -uc -b
	fi
	ls -la ../rust-xpra*deb
	cp ../rust-xpra*deb ../rust-xpra*changes "$REPO_ARCH_PATH"
fi
popd
