#!/usr/bin/env bash
# Re-exec under bash if started by sh/dash (e.g. `sh build-ubuntu2404.sh`).
if [ -z "${BASH_VERSION:-}" ]; then
	exec bash "$0" "$@"
fi

# Builds SentryShot for Ubuntu 24.04 (noble), x86_64, optimized for Intel Celeron
# N4020 (Goldmont Plus, SSE4.2, no AVX).
#
# The build runs natively inside a noble chroot created with debootstrap, so the
# binaries link against the same glibc (2.39) and ffmpeg 6.1 (libavcodec.so.60,
# libavutil.so.58) as the target server. Works on a Debian/Ubuntu host of any
# architecture that can run the chroot natively (x86_64 is required for the
# native build; cross builds are not supported by this script).
#
#     ./misc/build-ubuntu2404.sh
#
# Output in build/:
#     sentryshot-<version>-x86_64-ubuntu2404.tar.gz       binaries + libs + install.sh
#     sentryshot-<version>-x86_64-ubuntu2404-bin.tar.gz   sentryshot + plugins only
#
# Environment overrides:
#     CHROOT=/opt/noble-amd64   location of the noble chroot
#     RUST_TOOLCHAIN=1.85.0     rust toolchain inside the chroot
#     JOBS=N                    cargo parallel jobs (default: nproc)
#     MIRROR=...                ubuntu archive mirror for debootstrap
#     SKIP_CHROOT_SETUP=1       reuse an existing chroot without apt/rust steps

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SUITE=noble
CHROOT=${CHROOT:-/opt/noble-amd64}
RUST_TOOLCHAIN=${RUST_TOOLCHAIN:-1.85.0}
JOBS=${JOBS:-$(nproc)}
MIRROR=${MIRROR:-http://archive.ubuntu.com/ubuntu}
SKIP_CHROOT_SETUP=${SKIP_CHROOT_SETUP:-0}
CPU=goldmont-plus
RUST_TRIPLE=x86_64-unknown-linux-gnu

WORK=/work                       # inside the chroot
SRC_IN=$WORK/sentryshot          # project copy inside the chroot
TARGET_IN=$WORK/target           # cargo target dir inside the chroot

PLUGINS=(auth_basic auth_none motion mqtt object_detection object_detection_cv thumb_scale)

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n1)
BUILD_DIR="$ROOT/build"
STAGE="$BUILD_DIR/x86_64-ubuntu2404"
FULL_TAR="$BUILD_DIR/sentryshot-$VERSION-x86_64-ubuntu2404.tar.gz"
BIN_TAR="$BUILD_DIR/sentryshot-$VERSION-x86_64-ubuntu2404-bin.tar.gz"

log() { printf '\n==> %s\n' "$*"; }
die() { echo "error: $*" >&2; exit 1; }

root() {
	if [ "$(id -u)" -ne 0 ]; then sudo "$@"; else "$@"; fi
}

# Runs a bash snippet inside the chroot with a clean environment.
in_chroot() {
	root chroot "$CHROOT" /usr/bin/env -i \
		HOME=/root \
		PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
		LANG=C.UTF-8 \
		bash -euo pipefail -c "$1"
}

check_host() {
	[ "$(uname -m)" = "x86_64" ] || die "native build needs an x86_64 host, got $(uname -m)"
	command -v apt-get >/dev/null 2>&1 || die "apt-get required (Debian/Ubuntu host)"
	local mem_kb swap_kb
	mem_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo)
	swap_kb=$(awk '/SwapTotal/ {print $2}' /proc/meminfo)
	if [ $((mem_kb + swap_kb)) -lt $((6 * 1024 * 1024)) ]; then
		echo "warning: RAM+swap is below 6 GiB, the release build may run out of memory." >&2
	fi
	if [ "$(id -u)" -ne 0 ]; then
		sudo -v || die "sudo is required"
	fi
}

ensure_debootstrap() {
	if ! command -v debootstrap >/dev/null 2>&1; then
		log "installing debootstrap"
		root apt-get update -qq
		root env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq debootstrap
	fi
	# Older debootstrap releases lack the noble script; gutsy is the same Ubuntu layout.
	if [ ! -e "/usr/share/debootstrap/scripts/$SUITE" ]; then
		root ln -sf /usr/share/debootstrap/scripts/gutsy "/usr/share/debootstrap/scripts/$SUITE"
	fi
}

create_chroot() {
	if [ -f "$CHROOT/etc/os-release" ] && grep -q 'VERSION_CODENAME=noble' "$CHROOT/etc/os-release"; then
		log "using existing chroot $CHROOT"
		return
	fi
	log "creating $SUITE chroot in $CHROOT (debootstrap)"
	ensure_debootstrap
	root debootstrap --arch=amd64 --variant=minbase --include=ca-certificates \
		"$SUITE" "$CHROOT" "$MIRROR"
}

setup_apt() {
	log "configuring apt sources in chroot"
	root tee "$CHROOT/etc/apt/sources.list.d/ubuntu.sources" >/dev/null <<EOF
Types: deb
URIs: $MIRROR/
Suites: $SUITE $SUITE-updates $SUITE-security
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
	root rm -f "$CHROOT/etc/apt/sources.list"
	root cp /etc/resolv.conf "$CHROOT/etc/resolv.conf"
}

install_packages() {
	log "installing build dependencies in chroot"
	in_chroot "export DEBIAN_FRONTEND=noninteractive
		apt-get update -qq
		apt-get install -y -qq \
			build-essential pkgconf git curl ca-certificates \
			clang libclang-dev \
			libavcodec-dev libavutil-dev libusb-1.0-0-dev >/dev/null"
}

install_rust() {
	if [ -x "$CHROOT/root/.cargo/bin/cargo" ]; then
		log "rust already installed in chroot"
	else
		log "installing rustup + toolchain $RUST_TOOLCHAIN in chroot"
		in_chroot "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
			sh -s -- -y --profile minimal --default-toolchain $RUST_TOOLCHAIN"
	fi
	in_chroot "cargo --version && rustc --version"
}

sync_sources() {
	log "copying project sources into chroot"
	root rm -rf "$CHROOT$SRC_IN"
	root mkdir -p "$CHROOT$SRC_IN" "$CHROOT$TARGET_IN"
	# Build outputs and caches stay out of the copy; the cargo target is kept in the chroot.
	tar -C "$ROOT" \
		--exclude=./target --exclude=./target2 --exclude=./build \
		--exclude=./.git --exclude=./node_modules --exclude=./storage \
		-cf - . | root tar -C "$CHROOT$SRC_IN" -xf -
}

build() {
	log "building release for $CPU ($RUST_TRIPLE, jobs=$JOBS)"
	local pkgs="-p sentryshot"
	for p in "${PLUGINS[@]}"; do
		pkgs+=" -p $p"
	done
	# Rpath lets the binary and plugins find bundled libs in ./libs without patchelf.
	in_chroot "cd $SRC_IN
		export CARGO_TARGET_DIR=$TARGET_IN
		export RUSTFLAGS='-C target-cpu=$CPU -C link-args=-Wl,-rpath,\$ORIGIN/libs:\$ORIGIN/../libs'
		cargo build --release --locked -j $JOBS $pkgs"
}

stage() {
	log "staging files into $STAGE"
	local rel="$CHROOT$TARGET_IN/release"
	root rm -rf "$STAGE"
	mkdir -p "$STAGE/plugins" "$STAGE/libs"
	root cp "$rel/sentryshot" "$STAGE/sentryshot"
	for p in "${PLUGINS[@]}"; do
		root cp "$rel/lib$p.so" "$STAGE/plugins/libsentryshot_$p.so"
	done
	cp "$ROOT/misc/dist/install.sh" "$STAGE/install.sh"
	root chown -R "$(id -u):$(id -g)" "$STAGE"
	chmod 755 "$STAGE/sentryshot" "$STAGE/install.sh"
	chmod 644 "$STAGE"/plugins/*.so
}

# Library sonames the ELF file depends on (readelf is arch-agnostic).
needed_libs() {
	readelf -d "$1" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p'
}

# glibc, loader and libgcc come from the target system, never bundled.
is_system_lib() {
	case "$1" in
	libc.so.* | libm.so.* | libdl.so.* | libpthread.so.* | librt.so.* | \
		libresolv.so.* | libutil.so.* | libgcc_s.so.* | ld-linux*) return 0 ;;
	*) return 1 ;;
	esac
}

bundle_libs() {
	log "bundling non-glibc shared libraries (recursive)"
	local libdir="$CHROOT/usr/lib/x86_64-linux-gnu"
	local queue=("$STAGE/sentryshot" "$STAGE"/plugins/*.so)
	local seen=" "
	local f name
	while [ "${#queue[@]}" -gt 0 ]; do
		f=${queue[0]}
		queue=("${queue[@]:1}")
		while IFS= read -r name; do
			[ -n "$name" ] || continue
			is_system_lib "$name" && continue
			case "$seen" in *" $name "*) continue ;; esac
			seen="$seen$name "
			[ -e "$libdir/$name" ] || die "library $name not found in chroot"
			root cp -L "$libdir/$name" "$STAGE/libs/$name"
			root chown "$(id -u):$(id -g)" "$STAGE/libs/$name"
			chmod 644 "$STAGE/libs/$name"
			echo "  + $name"
			queue+=("$STAGE/libs/$name")
		done < <(needed_libs "$f")
	done
}

verify() {
	log "verifying bundle"
	local f name missing=0
	for f in "$STAGE/sentryshot" "$STAGE"/plugins/*.so "$STAGE"/libs/*; do
		while IFS= read -r name; do
			[ -n "$name" ] || continue
			if [ ! -e "$STAGE/libs/$name" ] && ! is_system_lib "$name"; then
				echo "  missing: $name (needed by $(basename "$f"))" >&2
				missing=1
			fi
		done < <(needed_libs "$f")
	done
	[ "$missing" -eq 0 ] || die "bundle is incomplete"

	readelf -h "$STAGE/sentryshot" | grep -q 'Machine:.*X86-64' || die "sentryshot is not x86_64"
	local glibc
	glibc=$(objdump -T "$STAGE/sentryshot" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -n1)
	echo "  max glibc symbol version: $glibc"

	# Run the binary inside the noble chroot from a copy of the staged tree.
	root rm -rf "$CHROOT/tmp/sentryshot-verify"
	root cp -a "$STAGE" "$CHROOT/tmp/sentryshot-verify"
	in_chroot "cd /tmp/sentryshot-verify && ./sentryshot --version && ldd ./sentryshot ./plugins/*.so | (! grep 'not found')"
	root rm -rf "$CHROOT/tmp/sentryshot-verify"
}

package() {
	log "creating archives"
	tar -czf "$FULL_TAR" -C "$STAGE" .
	tar -czf "$BIN_TAR" -C "$STAGE" sentryshot plugins
	(cd "$BUILD_DIR" && sha256sum "$(basename "$FULL_TAR")" "$(basename "$BIN_TAR")" \
		| tee "$(basename "$FULL_TAR").sha256" >/dev/null && cat "$(basename "$FULL_TAR").sha256")
	(cd "$BUILD_DIR" && sha256sum "$(basename "$BIN_TAR")" > "$(basename "$BIN_TAR").sha256")
	ls -lh "$FULL_TAR" "$BIN_TAR"
}

main() {
	check_host
	mkdir -p "$BUILD_DIR"
	if [ "$SKIP_CHROOT_SETUP" != "1" ]; then
		create_chroot
		setup_apt
		install_packages
		install_rust
	fi
	sync_sources
	build
	stage
	bundle_libs
	verify
	package
	log "done"
}

main "$@"
