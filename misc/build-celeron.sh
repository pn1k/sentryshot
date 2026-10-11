#!/usr/bin/env bash
# Re-exec under bash if started by sh/dash (e.g. `sh build-celeron.sh`).
if [ -z "${BASH_VERSION:-}" ]; then
	exec bash "$0" "$@"
fi

# Cross-builds SentryShot for Intel Celeron N4020 (Gemini Lake, Goldmont Plus,
# x86_64, SSE4.2, no AVX) from an arm64 or amd64 Debian/Ubuntu host, then packages
# a tarball with binaries and the shared libraries they need.
#
#     ./misc/build-celeron.sh
#
# Environment overrides:
#     SKIP_DEPS=1         do not install apt packages / rust target
#     RUST_TOOLCHAIN=...  rust toolchain to use (default: 1.85.0)
#     JOBS=N              parallel jobs for cargo (default: nproc)

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

TARGET_NAME=x86_64-celeron
RUST_TARGET=x86_64-unknown-linux-gnu
CPU=goldmont-plus
RUST_TOOLCHAIN=${RUST_TOOLCHAIN:-1.85.0}
JOBS=${JOBS:-$(nproc)}
SKIP_DEPS=${SKIP_DEPS:-0}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}

PLUGINS=(auth_basic auth_none motion mqtt object_detection object_detection_cv thumb_scale)

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
REL="$TARGET_DIR/$RUST_TARGET/release"
OUT="$ROOT/build/$TARGET_NAME"
TARBALL="$ROOT/build/sentryshot-$VERSION-$TARGET_NAME.tar.gz"

log() { printf '\n==> %s\n' "$*"; }
die() { echo "error: $*" >&2; exit 1; }

check_host() {
	command -v apt-get >/dev/null 2>&1 || die "apt-get required (Debian/Ubuntu host)"
	local mem_kb swap_kb
	mem_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo)
	swap_kb=$(awk '/SwapTotal/ {print $2}' /proc/meminfo)
	if [ $((mem_kb + swap_kb)) -lt $((4 * 1024 * 1024)) ]; then
		echo "warning: RAM+swap is below 4 GiB, the build may run out of memory." >&2
	fi
}

sudo_cmd() {
	if [ "$(id -u)" -ne 0 ]; then sudo "$@"; else "$@"; fi
}

# Foreign arch and cross toolchain depend on the host arch.
install_deps() {
	log "installing cross build dependencies"
	local host_arch
	host_arch=$(dpkg --print-architecture)
	if [ "$host_arch" = "amd64" ]; then
		# Native x86_64 host: no cross gcc needed.
		local cross_gcc=""
	else
		sudo_cmd dpkg --add-architecture amd64
		local cross_gcc="gcc-x86-64-linux-gnu g++-x86-64-linux-gnu"
	fi
	sudo_cmd apt-get update
	# shellcheck disable=SC2086
	sudo_cmd apt-get install -y \
		build-essential pkgconf git curl ca-certificates \
		$cross_gcc \
		libc6-dev:amd64 \
		libavcodec-dev:amd64 libavutil-dev:amd64 \
		libusb-1.0-0-dev:amd64 \
		libopenh264-dev:amd64 libopenh264-7:amd64
}

ensure_rust() {
	if ! command -v cargo >/dev/null 2>&1; then
		log "installing rustup + toolchain $RUST_TOOLCHAIN"
		curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
			sh -s -- -y --profile minimal --default-toolchain "$RUST_TOOLCHAIN"
		# shellcheck disable=SC1091
		. "$HOME/.cargo/env"
	fi
	rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal
	rustup target add --toolchain "$RUST_TOOLCHAIN" "$RUST_TARGET"
	cargo --version
}

# Points cargo/pkg-config at the amd64 sysroot and cross linker.
setup_cross_env() {
	local triple_env=X86_64_UNKNOWN_LINUX_GNU
	local host_arch
	host_arch=$(dpkg --print-architecture)
	if [ "$host_arch" = "amd64" ]; then
		export CC_x86_64_unknown_linux_gnu=gcc
	else
		export CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc
		export "CARGO_TARGET_${triple_env}_LINKER=x86_64-linux-gnu-gcc"
	fi
	export PKG_CONFIG_ALLOW_CROSS=1
	export PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig
	export PKG_CONFIG_LIBDIR=/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig
	export CARGO_TARGET_DIR="$TARGET_DIR"
	# Rpath lets binary and plugins find bundled libs in ./libs without patchelf.
	export "CARGO_TARGET_${triple_env}_RUSTFLAGS=-C target-cpu=$CPU -C link-args=-Wl,-rpath,\$ORIGIN/libs:\$ORIGIN/../libs"
}

build() {
	log "building release for $CPU ($RUST_TARGET, jobs=$JOBS)"
	local pkgs=(-p sentryshot)
	for p in "${PLUGINS[@]}"; do
		pkgs+=(-p "$p")
	done
	cargo build --release --locked --target "$RUST_TARGET" -j "$JOBS" "${pkgs[@]}"
}

stage() {
	log "staging files into $OUT"
	rm -rf "$OUT"
	mkdir -p "$OUT/plugins" "$OUT/libs"
	cp "$REL/sentryshot" "$OUT/sentryshot"
	for p in "${PLUGINS[@]}"; do
		cp "$REL/lib$p.so" "$OUT/plugins/libsentryshot_$p.so"
	done
	cp "$ROOT/misc/dist/install.sh" "$OUT/install.sh"
	chmod 755 "$OUT/sentryshot" "$OUT/install.sh"
	chmod 644 "$OUT"/plugins/*.so
}

# Reads NEEDED entries with readelf (works for foreign-arch ELF; ldd does not).
needed_libs() {
	x86_64-linux-gnu-readelf -d "$1" 2>/dev/null | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p'
}

# glibc and loader come from the target system, never bundled.
is_system_lib() {
	case "$1" in
	libc.so.* | libm.so.* | libdl.so.* | libpthread.so.* | librt.so.* | \
		libresolv.so.* | libutil.so.* | libgcc_s.so.* | ld-linux*) return 0 ;;
	*) return 1 ;;
	esac
}

# Finds a library by soname in the amd64 multiarch dir.
find_lib() {
	local name=$1
	local path="/usr/lib/x86_64-linux-gnu/$name"
	[ -e "$path" ] || die "library $name not found in /usr/lib/x86_64-linux-gnu"
	readlink -f "$path"
}

bundle_libs() {
	log "bundling non-glibc shared libraries (recursive)"
	local queue=("$OUT/sentryshot" "$OUT"/plugins/*.so)
	local seen=" "
	local f lib name
	while [ "${#queue[@]}" -gt 0 ]; do
		f=${queue[0]}
		queue=("${queue[@]:1}")
		while IFS= read -r name; do
			[ -n "$name" ] || continue
			is_system_lib "$name" && continue
			case "$seen" in *" $name "*) continue ;; esac
			seen="$seen$name "
			lib=$(find_lib "$name")
			cp -L "$lib" "$OUT/libs/$name"
			echo "  + $name"
			queue+=("$OUT/libs/$name")
		done < <(needed_libs "$f")
	done
	chmod 644 "$OUT"/libs/* 2>/dev/null || true
}

verify_bundle() {
	log "verifying bundle"
	local f name missing=0
	for f in "$OUT/sentryshot" "$OUT"/plugins/*.so "$OUT"/libs/*; do
		while IFS= read -r name; do
			[ -n "$name" ] || continue
			if [ ! -e "$OUT/libs/$name" ] && ! is_system_lib "$name"; then
				echo "  missing: $name (needed by $(basename "$f"))" >&2
				missing=1
			fi
		done < <(needed_libs "$f")
	done
	[ "$missing" -eq 0 ] || die "bundle is incomplete"
	x86_64-linux-gnu-readelf -h "$OUT/sentryshot" | grep -q "Machine:.*X86-64" \
		|| die "sentryshot is not an x86_64 binary"
}

package() {
	log "creating $TARBALL"
	tar -czf "$TARBALL" -C "$OUT" .
	(cd "$ROOT/build" && sha256sum "$(basename "$TARBALL")" >"$(basename "$TARBALL").sha256")
	cat "$TARBALL.sha256"
	ls -lh "$TARBALL"
}

main() {
	check_host
	if [ "$SKIP_DEPS" != "1" ]; then
		install_deps
		ensure_rust
	fi
	setup_cross_env
	build
	stage
	bundle_libs
	verify_bundle
	package
	log "done: $TARBALL"
}

main "$@"
