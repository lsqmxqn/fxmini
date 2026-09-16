#!/usr/bin/env bash
# FxMini — Git Bash / MSYS equivalent of toolchain.ps1.
#
#   source toolchain.sh
#   cargo build --release --bin dspcheck
#
# See toolchain.ps1 for the full explanation. The short version: this machine's
# Visual Studio lives at D:\Program Files\VisualStudio, is not registered with
# the installer, and Git for Windows ships a coreutils `link.exe` that shadows
# MSVC's linker. Both are handled by prepending the MSVC bin directory to PATH
# and exporting INCLUDE/LIB explicitly.

FXMINI_MSVC_ROOT="${FXMINI_MSVC_ROOT:-/d/Program Files/VisualStudio/VC/Tools/MSVC}"
FXMINI_SDK_ROOT="${FXMINI_SDK_ROOT:-/c/Program Files (x86)/Windows Kits/10}"
FXMINI_ARCH="${FXMINI_ARCH:-x64}"
FXMINI_HOST_ARCH="${FXMINI_HOST_ARCH:-Hostx64}"

_fxmini_fail() { echo "toolchain.sh: $*" >&2; return 1; }

# Pick the newest version that actually contains the piece we need, so a
# partially installed toolset (headers but no libraries) is never selected.
_fxmini_newest() {
    local root="$1" probe="$2"
    [ -d "$root" ] || _fxmini_fail "not found: $root" || return 1
    local best="" best_key=""
    local dir name key
    for dir in "$root"/*/; do
        [ -d "$dir" ] || continue
        if [ -n "$probe" ] && [ ! -e "$dir$probe" ]; then continue; fi
        name="$(basename "$dir")"
        key="$(printf '%s' "$name" | tr -cd '0-9.')"
        if [ -z "$best_key" ] || [ "$(printf '%s\n%s\n' "$best_key" "$key" | sort -V | tail -1)" = "$key" ]; then
            best="$dir"; best_key="$key"
        fi
    done
    [ -n "$best" ] || { _fxmini_fail "no usable version under $root"; return 1; }
    printf '%s' "${best%/}"
}

FXMINI_MSVC="$(_fxmini_newest "$FXMINI_MSVC_ROOT" "lib/$FXMINI_ARCH")" || return 1
FXMINI_SDK_INC="$(_fxmini_newest "$FXMINI_SDK_ROOT/Include" "")" || return 1
FXMINI_SDK_VER="$(basename "$FXMINI_SDK_INC")"

[ -d "$FXMINI_SDK_ROOT/Lib/$FXMINI_SDK_VER/um/$FXMINI_ARCH" ] \
    || _fxmini_fail "SDK $FXMINI_SDK_VER has no Lib/.../um/$FXMINI_ARCH" || return 1

FXMINI_VC_BIN="$FXMINI_MSVC/bin/$FXMINI_HOST_ARCH/$FXMINI_ARCH"
[ -x "$FXMINI_VC_BIN/cl.exe" ] || _fxmini_fail "cl.exe not found in $FXMINI_VC_BIN" || return 1

# MSVC's bin MUST come first, otherwise coreutils' link.exe /usr/bin/link.exe wins.
export PATH="$FXMINI_VC_BIN:$FXMINI_SDK_ROOT/bin/$FXMINI_SDK_VER/$FXMINI_ARCH:$PATH"

# cl.exe and link.exe read Windows-style paths from these.
_fxmini_win() { printf '%s' "$1" | sed 's|^/\([a-z]\)/|\U\1:\\|; s|/|\\|g'; }

export INCLUDE="$(_fxmini_win "$FXMINI_MSVC/include");$(_fxmini_win "$FXMINI_SDK_ROOT/Include/$FXMINI_SDK_VER/ucrt");$(_fxmini_win "$FXMINI_SDK_ROOT/Include/$FXMINI_SDK_VER/um");$(_fxmini_win "$FXMINI_SDK_ROOT/Include/$FXMINI_SDK_VER/shared");$(_fxmini_win "$FXMINI_SDK_ROOT/Include/$FXMINI_SDK_VER/winrt");$(_fxmini_win "$FXMINI_SDK_ROOT/Include/$FXMINI_SDK_VER/cppwinrt")"

export LIB="$(_fxmini_win "$FXMINI_MSVC/lib/$FXMINI_ARCH");$(_fxmini_win "$FXMINI_SDK_ROOT/Lib/$FXMINI_SDK_VER/ucrt/$FXMINI_ARCH");$(_fxmini_win "$FXMINI_SDK_ROOT/Lib/$FXMINI_SDK_VER/um/$FXMINI_ARCH")"

# cc-rs probes vswhere, which cannot see this installation; point it at cl.exe directly.
export CC=cl.exe
export CXX=cl.exe

_which_link="$(command -v link.exe)"
echo "FxMini toolchain ready"
echo "  MSVC toolset : $FXMINI_MSVC"
echo "  Windows SDK  : $FXMINI_SDK_VER"
echo "  cl.exe       : $FXMINI_VC_BIN/cl.exe"
echo "  link.exe     : $_which_link"
case "$_which_link" in
    "$FXMINI_VC_BIN"/*) ;;
    *) echo "  WARNING: link.exe is not MSVC's — linking will fail." >&2 ;;
esac
echo
echo "Next: cargo build --release --bin dspcheck"

unset _which_link _fxmini_fail
