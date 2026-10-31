#!/usr/bin/env bash
# x86_64 Dolby interop toolchain for salsa.
#
# The Dolby Reference Player ships an x86_64 GStreamer 1.18.2 runtime and
# x86_64-only Dolby plugins. Salsa builds natively for arm64, so a GStreamer
# process cannot load both. This script builds salsa for x86_64 and links it
# against the Reference Player runtime, so the Dolby plugins and salsa elements
# can be loaded by the same process (under Rosetta).
#
# Usage:
#   source tools/x86-interop-env.sh
#   tools/build-x86-plugins.sh
#
# Everything is cached under $SALSA_X86_CACHE (default ~/.cache/salsa-x86).
# No files inside the Dolby application bundle are modified.

# shellcheck shell=bash

# Support both `source` from bash and from an interactive zsh (BASH_SOURCE is
# empty there). If the source path is unavailable, assume the caller is in the
# repository root.
_salsa_self="${BASH_SOURCE[0]:-}"
if [[ -z "$_salsa_self" && -f "$PWD/tools/x86-interop-env.sh" ]]; then
    _salsa_self="$PWD/tools/x86-interop-env.sh"
fi
if [[ -z "$_salsa_self" ]]; then
    echo "Source tools/x86-interop-env.sh from the repository root." >&2
    return 1 2>/dev/null || exit 1
fi
SALSA_REPO_ROOT="$(cd "$(dirname "$_salsa_self")/.." && pwd)"
unset _salsa_self
SALSA_X86_CACHE="${SALSA_X86_CACHE:-$HOME/.cache/salsa-x86}"
SALSA_DRP_APP="${SALSA_DRP_APP:-/Applications/Dolby/Dolby Reference Player.app}"
SALSA_DRP_RESOURCES="${SALSA_DRP_RESOURCES:-$SALSA_DRP_APP/Contents/Frameworks/GStreamer.framework/Versions/1_16/Resources}"
SALSA_GST_DEVEL_VERSION="${SALSA_GST_DEVEL_VERSION:-1.18.6}"
SALSA_GST_DEVEL_PKG="$SALSA_X86_CACHE/src/gstreamer-1.0-devel-${SALSA_GST_DEVEL_VERSION}-x86_64.pkg"
SALSA_GST_DEVEL_URL="${SALSA_GST_DEVEL_URL:-https://gstreamer.freedesktop.org/data/pkg/osx/${SALSA_GST_DEVEL_VERSION}/gstreamer-1.0-devel-${SALSA_GST_DEVEL_VERSION}-x86_64.pkg}"
SALSA_FFMPEG_VERSION="${SALSA_FFMPEG_VERSION:-9.0.2}"
SALSA_FFMPEG_TARBALL="$SALSA_X86_CACHE/src/ffmpeg-${SALSA_FFMPEG_VERSION}.tar.xz"
SALSA_FFMPEG_URL="${SALSA_FFMPEG_URL:-https://ffmpeg.org/releases/ffmpeg-${SALSA_FFMPEG_VERSION}.tar.xz}"
SALSA_GST_PREFIX="$SALSA_X86_CACHE/gst/prefix"
SALSA_FFMPEG_PREFIX="$SALSA_X86_CACHE/ffmpeg"
SALSA_DRP_LINK="$SALSA_X86_CACHE/drp-gst"
SALSA_X86_PLUGINS="$SALSA_X86_CACHE/plugins"
SALSA_RUSTUP_TOOLCHAIN="${SALSA_RUSTUP_TOOLCHAIN:-$(rustup show active-toolchain 2>/dev/null | awk '{print $1}')}"
SALSA_GST_PLUGIN_OAMD_PATH="${SALSA_GST_PLUGIN_OAMD_PATH:-$SALSA_REPO_ROOT/../gst-plugin-oamd/build/cmake-x86_64/reference-player-plugins}"

if [[ ! -d "$SALSA_DRP_RESOURCES" ]]; then
    echo "Dolby Reference Player GStreamer runtime not found at:" >&2
    echo "  $SALSA_DRP_RESOURCES" >&2
    echo "Set SALSA_DRP_RESOURCES to the framework's Resources directory." >&2
    return 1 2>/dev/null || exit 1
fi

if [[ -z "$SALSA_RUSTUP_TOOLCHAIN" ]]; then
    echo "rustup with an active toolchain is required for x86_64 cross builds." >&2
    return 1 2>/dev/null || exit 1
fi
if ! rustup target list --installed --toolchain "$SALSA_RUSTUP_TOOLCHAIN" | grep -qx x86_64-apple-darwin; then
    echo "Installing the x86_64-apple-darwin target for $SALSA_RUSTUP_TOOLCHAIN" >&2
    rustup target add x86_64-apple-darwin --toolchain "$SALSA_RUSTUP_TOOLCHAIN"
fi
SALSA_X86_RUSTC="$(rustup which --toolchain "$SALSA_RUSTUP_TOOLCHAIN" rustc)"
SALSA_X86_RUSTDOC="$(rustup which --toolchain "$SALSA_RUSTUP_TOOLCHAIN" rustdoc)"
SALSA_X86_CARGO="$(rustup which --toolchain "$SALSA_RUSTUP_TOOLCHAIN" cargo)"

mkdir -p "$SALSA_X86_CACHE/src" "$SALSA_X86_CACHE/plugins"

write_pc() {
    local name="$1"
    local version="$2"
    local requires="$3"
    local requires_private="$4"
    local libs="$5"
    local cflags="$6"
    cat >"$SALSA_GST_PREFIX/lib/pkgconfig/$name.pc" <<EOF
prefix=$SALSA_GST_PREFIX
exec_prefix=\${prefix}
libdir=$SALSA_DRP_LINK/lib
includedir=\${prefix}/include

Name: $name
Description: x86_64 Dolby Reference Player GStreamer runtime
Version: $version
Requires: $requires
Requires.private: $requires_private
Libs: -L\${libdir} $libs -Wl,-rpath,$SALSA_DRP_LINK
Cflags: $cflags
EOF
}

setup_gstreamer_headers() {
    if [[ -f "$SALSA_GST_PREFIX/lib/pkgconfig/gstreamer-audio-1.0.pc" ]]; then
        return
    fi
    echo "Preparing GStreamer ${SALSA_GST_DEVEL_VERSION} x86_64 headers..." >&2
    if [[ ! -f "$SALSA_GST_DEVEL_PKG" ]]; then
        curl -L --fail --retry 3 -o "$SALSA_GST_DEVEL_PKG" "$SALSA_GST_DEVEL_URL"
    fi
    local extract="$SALSA_X86_CACHE/src/gst-devel"
    rm -rf "$extract"
    mkdir -p "$extract"
    (
        cd "$extract"
        xar -xf "$SALSA_GST_DEVEL_PKG"
        for package in \
            "gstreamer-1.0-core-devel-${SALSA_GST_DEVEL_VERSION}-x86_64.pkg" \
            "base-system-1.0-devel-${SALSA_GST_DEVEL_VERSION}-x86_64.pkg"; do
            local dir="${package%.pkg}"
            mkdir -p "$dir"
            (
                cd "$dir"
                gunzip -dc "../$package/Payload" | cpio -id 2>/dev/null
            )
        done
    )

    local core="$extract/gstreamer-1.0-core-devel-${SALSA_GST_DEVEL_VERSION}-x86_64"
    local base="$extract/base-system-1.0-devel-${SALSA_GST_DEVEL_VERSION}-x86_64"
    rm -rf "$SALSA_GST_PREFIX"
    mkdir -p "$SALSA_GST_PREFIX/include" "$SALSA_GST_PREFIX/lib/pkgconfig" \
        "$SALSA_GST_PREFIX/lib/glib-2.0"
    cp -R "$core/include/gstreamer-1.0/." "$SALSA_GST_PREFIX/include/"
    cp -R "$base/include/." "$SALSA_GST_PREFIX/include/"
    cp -R "$base/lib/glib-2.0/include" "$SALSA_GST_PREFIX/lib/glib-2.0/include"

    write_pc glib-2.0 2.62.6 "" "" "-lglib-2.0 -lintl" \
        "-I\${includedir}/glib-2.0 -I\${prefix}/lib/glib-2.0/include"
    write_pc gobject-2.0 2.62.6 "glib-2.0" "" "-lgobject-2.0" \
        "-I\${includedir}/glib-2.0"
    write_pc gmodule-no-export-2.0 2.62.6 "glib-2.0" "" "-lgmodule-2.0" \
        "-I\${includedir}/glib-2.0"
    write_pc gio-2.0 2.62.6 "glib-2.0, gobject-2.0" "gmodule-no-export-2.0" \
        "-lgio-2.0" "-I\${includedir}/glib-2.0 -I\${prefix}/lib/glib-2.0/include"
    write_pc gio-unix-2.0 2.62.6 "gio-2.0" "" "" "-I\${includedir}/gio-unix-2.0"
    write_pc gthread-2.0 2.62.6 "glib-2.0" "" "-lgthread-2.0" \
        "-I\${includedir}/glib-2.0 -I\${prefix}/lib/glib-2.0/include"
    write_pc gstreamer-1.0 1.18.2 "glib-2.0, gobject-2.0" \
        "gmodule-no-export-2.0" "-lgstreamer-1.0" "-I\${includedir}/gstreamer-1.0"
    write_pc gstreamer-tag-1.0 1.18.2 "gstreamer-1.0" "" "-lgsttag-1.0" \
        "-I\${includedir}/gstreamer-1.0"
    write_pc gstreamer-base-1.0 1.18.2 "gstreamer-1.0" "" "-lgstbase-1.0" \
        "-I\${includedir}/gstreamer-1.0"
    write_pc gstreamer-audio-1.0 1.18.2 \
        "gstreamer-1.0, gstreamer-base-1.0, gstreamer-tag-1.0" "" \
        "-lgstaudio-1.0" "-I\${includedir}/gstreamer-1.0"
}

setup_ffmpeg() {
    if [[ -f "$SALSA_FFMPEG_PREFIX/lib/pkgconfig/libavcodec.pc" ]]; then
        return
    fi
    echo "Building FFmpeg ${SALSA_FFMPEG_VERSION} for x86_64 (minimal E-AC-3)..." >&2
    if [[ ! -f "$SALSA_FFMPEG_TARBALL" ]]; then
        curl -L --fail --retry 3 -o "$SALSA_FFMPEG_TARBALL" "$SALSA_FFMPEG_URL"
    fi
    local source="$SALSA_X86_CACHE/src/ffmpeg-${SALSA_FFMPEG_VERSION}"
    local build="$SALSA_X86_CACHE/ffmpeg-build"
    rm -rf "$source" "$build"
    tar -C "$SALSA_X86_CACHE/src" -xf "$SALSA_FFMPEG_TARBALL"
    mkdir -p "$build"
    (
        cd "$build"
        "$source/configure" \
            --prefix="$SALSA_FFMPEG_PREFIX" \
            --arch=x86_64 --target-os=darwin --enable-cross-compile \
            --cc="clang -arch x86_64" --cxx="clang++ -arch x86_64" \
            --disable-autodetect --disable-programs --disable-doc \
            --disable-avdevice --disable-avfilter --disable-swscale \
            --disable-swresample --disable-network --disable-everything \
            --enable-decoder=ac3,eac3 --enable-parser=ac3 \
            --enable-shared --disable-static --disable-x86asm \
            >"$SALSA_X86_CACHE/ffmpeg-configure.log" 2>&1
        make -j"$(sysctl -n hw.ncpu)" >"$SALSA_X86_CACHE/ffmpeg-make.log" 2>&1
        make install >"$SALSA_X86_CACHE/ffmpeg-install.log" 2>&1
    )
    # FFmpeg 9.1-dev added CUDA array members that ffmpeg-sys-next 9.0 cannot
    # bind. The header is optional; remove it so bindgen skips it.
    rm -f "$SALSA_FFMPEG_PREFIX/include/libavutil/hwcontext_cuda.h"
}

setup_gstreamer_headers
setup_ffmpeg
ln -sfn "$SALSA_DRP_RESOURCES" "$SALSA_DRP_LINK"

export SALSA_X86_CACHE SALSA_X86_PLUGINS SALSA_DRP_RESOURCES
export SALSA_GST_PLUGIN_OAMD_PATH
export PATH="$(dirname "$SALSA_X86_RUSTC"):$PATH"
export RUSTC="$SALSA_X86_RUSTC"
export RUSTDOC="$SALSA_X86_RUSTDOC"
export PKG_CONFIG_LIBDIR="$SALSA_GST_PREFIX/lib/pkgconfig:$SALSA_FFMPEG_PREFIX/lib/pkgconfig"
export PKG_CONFIG_PATH=""
export PKG_CONFIG_ALLOW_CROSS=1
export GST_PLUGIN_SCANNER="$SALSA_DRP_RESOURCES/libexec/gst-plugin-scanner"
export GST_PLUGIN_SYSTEM_PATH_1_0="$SALSA_DRP_RESOURCES/lib/gstreamer-1.0"
export GST_REGISTRY_FORK=no
export SALSA_GST_LAUNCH="$SALSA_DRP_RESOURCES/bin/gst-launch-1.0"
export SALSA_GST_INSPECT="$SALSA_DRP_RESOURCES/bin/gst-inspect-1.0"

echo "salsa x86 interop environment ready" >&2
echo "  runtime:  $SALSA_DRP_RESOURCES" >&2
echo "  cache:    $SALSA_X86_CACHE" >&2
echo "  toolchain: $SALSA_RUSTUP_TOOLCHAIN ($SALSA_X86_CARGO)" >&2
