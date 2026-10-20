#!/bin/sh
# Install a Dudes release: `duded`, `dudes` and `dudes-hook` into <prefix>/bin and the
# Floor into <prefix>/share/dudes/floor, where `duded` looks for it, and the licence notices into
# <prefix>/share/dudes. `sh install.sh --help` lists the options.
#
# The archive for this machine's target triple is checked against the release's SHA256SUMS before
# anything is installed. The script never uses sudo and never edits a shell profile.
#
# With `--from <directory>` the archive and SHA256SUMS come from a local directory, such as the
# `target/dist` that `cargo xtask dist` writes, and the script makes no network request.
#
# The script also runs as `curl -fsSL <url> | sh` and `curl -fsSL <url> | sh -s -- --version <v>`:
# `main` runs only after the shell has read the whole file, and nothing here reads standard input.
#
# Releases are tagged `v<version>` and carry `dudes-<version>-<target>.tar.gz` archives, as
# `cargo xtask dist` writes them, beside one SHA256SUMS file. The latest release's version is read
# from its SHA256SUMS, so resolving it needs neither the GitHub API nor a separate version file.

set -eu

repository=https://github.com/ArcfluxResearch/dudes
files='bin/duded bin/dudes bin/dudes-hook share/dudes/LICENSE-APACHE share/dudes/LICENSE-MIT share/dudes/THIRD-PARTY.md'
supported_targets='aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu aarch64-unknown-linux-musl x86_64-unknown-linux-musl'

usage() {
	cat <<'EOF'
Install a Dudes release, from GitHub or from a local build.

Usage: install.sh [--from <dir>] [--version <version>] [--prefix <dir>]

Options:
  --from <dir>         install from <dir>, the target/dist directory that `cargo xtask dist`
                       writes, instead of downloading; cannot be combined with
                       DUDES_INSTALL_BASE_URL
  --version <version>  install this release, such as 0.47.0, instead of the latest
  --prefix <dir>       install into <dir>/bin and <dir>/share/dudes (default: $HOME/.local)
  -h, --help           print this help

Environment:
  DUDES_INSTALL_BASE_URL  a directory URL holding a release's archives and SHA256SUMS, in place
                          of the project's GitHub releases: https, or http on 127.0.0.1 or
                          localhost for local testing

The script never uses sudo and never edits a shell profile. As root it installs only with an
explicit --prefix.
EOF
}

say() {
	printf '%s\n' "$*"
}

fail() {
	printf 'dudes install: %s\n' "$*" >&2
	exit 1
}

# Whether $1 is a release version: strict semantic versioning, as Cargo writes it.
is_version() {
	case $1 in
	'' | *[!0-9A-Za-z.+-]*) return 1 ;;
	esac
	printf '%s\n' "$1" |
		grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
}

target_triple() {
	system=$(uname -s)
	machine=$(uname -m)
	case $machine in
	x86_64) arch=x86_64 ;;
	arm64 | aarch64) arch=aarch64 ;;
	*) arch= ;;
	esac
	case $system in
	Darwin)
		os=apple-darwin
		# A shell under Rosetta reports x86_64 on Apple silicon, where the native build belongs.
		if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
			arch=aarch64
		fi
		;;
	# Every Linux host gets the static build, which runs on any distribution. The glibc build needs
	# the glibc it was built against, which an older distribution does not have.
	Linux) os=unknown-linux-musl ;;
	*) os= ;;
	esac
	if [ -z "$arch" ] || [ -z "$os" ]; then
		fail "unsupported platform: $system $machine; this installer supports macOS and Linux on x86_64 and arm64, and $repository/blob/main/docs/building-from-source.md builds from source"
	fi
	candidate=$arch-$os
	case " $supported_targets " in
	*" $candidate "*) ;;
	*) fail "unsupported platform: $system $machine; this installer supports macOS and Linux on x86_64 and arm64, and $repository/blob/main/docs/building-from-source.md builds from source" ;;
	esac
	printf '%s\n' "$candidate"
}

download() {
	curl --fail --silent --show-error --location --max-redirs "$redirects" --proto "$proto" \
		--proto-redir "$proto" --retry 3 --connect-timeout 30 --output "$2" "$1" ||
		fail "could not download $1"
}

# Put the release file named $1 at $2: copied from the --from directory, or downloaded.
fetch() {
	if [ -n "$from" ]; then
		[ -f "$from/$1" ] || fail "$from has no $1"
		cp "$from/$1" "$2" || fail "could not copy $from/$1"
	else
		download "$base/$1" "$2"
	fi
}

restore_file() {
	if cp -RPp "$rollback/$name" "$restore_dir/$name" &&
		mv -f "$restore_dir/$name" "$installed"; then
		return 0
	fi
	if rm -f "$installed" && cp -RPp "$rollback/$name" "$installed"; then
		rm -f "$restore_dir/$name"
		return 0
	fi
	: > "$rollback/rename_started.$name" || return 1
	mv -f "$rollback/$name" "$installed"
}

restore_previous() {
	restored=yes
	restore_dir=$(mktemp -d "$bin/.dudes-restore.XXXXXX") || return 1
	for file in $files; do
		name=${file##*/}
		installed=$prefix/$file
		if [ -e "$rollback/$name" ] || [ -L "$rollback/$name" ]; then
			restore_file || restored=no
		elif [ -e "$rollback/absent.$name" ]; then
			rm -f "$installed" || restored=no
		elif [ "$file" = "bin/$name" ] && [ ! -e "$installed" ] && [ ! -L "$installed" ]; then
			restored=no
		fi
	done
	rm -rf "$restore_dir" || restored=no
	if [ -e "$share/floor.old" ] || [ -L "$share/floor.old" ]; then
		if ! { rm -rf "$share/floor" &&
			{ cp -RPp "$share/floor.old" "$share/floor" ||
				{ rm -rf "$share/floor" && mv "$share/floor.old" "$share/floor"; }; }; }; then
			restored=no
		fi
	elif [ -e "$rollback/floor.was-absent" ]; then
		rm -rf "$share/floor" || restored=no
	fi
	[ "$restored" = yes ] || return 1
	rm -f "$rollback/ready" || return 1
	rm -rf "$rollback" "$share/floor.old" || return 1
}

valid_rollback() {
	[ ! -L "$rollback" ] && [ -d "$rollback" ] && [ -O "$rollback" ] || return 1
	[ "$(find "$rollback" -prune -perm 0700 -print)" = "$rollback" ] || return 1
	for entry in "$rollback"/* "$rollback"/.[!.]* "$rollback"/..?*; do
		[ -e "$entry" ] || [ -L "$entry" ] || continue
		name=${entry##*/}
		case $name in
		ready | floor.was-absent | floor.was-present)
			[ -f "$entry" ] && [ ! -L "$entry" ] || return 1
			continue
			;;
		absent.* | rename_started.*)
			name=${name#*.}
			[ -f "$entry" ] && [ ! -L "$entry" ] || return 1
			;;
		*) [ -f "$entry" ] || [ -L "$entry" ] || return 1 ;;
		esac
		case " $files " in
		*"/$name "*) ;;
		*) return 1 ;;
		esac
	done
	if [ -e "$rollback/ready" ]; then
		for file in $files; do
			name=${file##*/}
			installed=$prefix/$file
			if [ -e "$rollback/$name" ] || [ -L "$rollback/$name" ]; then
				[ ! -e "$rollback/absent.$name" ] || return 1
			elif [ -e "$rollback/rename_started.$name" ]; then
				[ ! -e "$rollback/absent.$name" ] || return 1
				[ -f "$installed" ] || [ -L "$installed" ] || return 1
			elif [ "$file" = "bin/$name" ]; then
				[ -e "$rollback/absent.$name" ] || return 1
			fi
		done
		if [ -e "$rollback/floor.was-absent" ]; then
			[ ! -e "$rollback/floor.was-present" ] || return 1
		else
			[ -e "$rollback/floor.was-present" ] || return 1
		fi
	fi
	return 0
}

# Add a "Dudes" launcher that runs `dudes open`: an app in ~/Applications on macOS, a desktop entry
# on Linux. It registers no URL scheme and no file type, so nothing outside the user's own menu can
# start it. Both carry the word DudesLauncher, which `dudes uninstall` looks for before it deletes
# one. A launcher that cannot be written never fails the install.
add_launcher() {
	[ -n "${HOME:-}" ] || return 0
	program=$bin/dudes
	case $program in
	*[\"\`\$\\]* | *'
'*) return 0 ;;
	esac
	case $(uname -s) in
	Darwin)
		app=$HOME/Applications/Dudes.app
		quoted=$(printf '%s' "$program" | sed "s/'/'\\\\''/g") || return 0
		{
			rm -rf "$app" &&
				mkdir -p "$app/Contents/MacOS" &&
				cat > "$app/Contents/Info.plist" <<EOF &&
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key><string>Dudes</string>
	<key>CFBundleIdentifier</key><string>dev.dudes.launcher</string>
	<key>CFBundleName</key><string>Dudes</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleVersion</key><string>1</string>
	<key>LSUIElement</key><true/>
	<key>DudesLauncher</key><true/>
</dict>
</plist>
EOF
				printf '#!/bin/sh\nexec '"'"'%s'"'"' open\n' "$quoted" > "$app/Contents/MacOS/Dudes" &&
				chmod 755 "$app/Contents/MacOS/Dudes"
		} 2>/dev/null || return 0
		launcher=$app
		;;
	Linux)
		applications=$HOME/.local/share/applications
		{
			mkdir -p "$applications" &&
				cat > "$applications/dudes.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Dudes
Comment=Open the Dudes Floor
Exec="$program" open
Terminal=false
Categories=Development;
X-DudesLauncher=true
EOF
		} 2>/dev/null || return 0
		launcher=$applications/dudes.desktop
		;;
	esac
}

main() {
	umask 022
	version=
	prefix=
	from=
	from_given=no
	while [ $# -gt 0 ]; do
		case $1 in
		--from)
			[ $# -ge 2 ] || fail "--from needs a value"
			from=$2
			from_given=yes
			shift 2
			;;
		--version)
			[ $# -ge 2 ] || fail "--version needs a value"
			version=${2#v}
			is_version "$version" || fail "not a release version: $2"
			shift 2
			;;
		--prefix)
			[ $# -ge 2 ] || fail "--prefix needs a value"
			prefix=$2
			shift 2
			;;
		-h | --help)
			usage
			exit 0
			;;
		*) fail "unknown argument: $1 (see --help)" ;;
		esac
	done

	if [ "$from_given" = yes ]; then
		[ -n "$from" ] || fail "--from needs a non-empty directory"
		[ -z "${DUDES_INSTALL_BASE_URL:-}" ] || fail "--from cannot be combined with DUDES_INSTALL_BASE_URL"
		[ -d "$from" ] || fail "$from is not a directory"
		from=$(CDPATH= cd -- "$from" && pwd) || fail "could not enter $from"
	fi

	target=$(target_triple) || exit 1

	if [ -z "$prefix" ]; then
		[ "$(id -u)" != 0 ] || fail "refusing to install as root without --prefix; run this as your own user, or pass --prefix <dir> to choose where"
		[ -n "${HOME:-}" ] || fail "HOME is not set; pass --prefix <dir>"
		prefix=$HOME/.local
	fi
	case $prefix in
	/*) ;;
	*) fail "the prefix must be an absolute path: $prefix" ;;
	esac
	bin=$prefix/bin
	share=$prefix/share/dudes
	rollback=
	for candidate in "$share"/.install-rollback.*; do
		[ -e "$candidate" ] || [ -L "$candidate" ] || continue
		[ -z "$rollback" ] || fail "multiple installation backups need operator attention"
		rollback=$candidate
	done
	if [ -n "$rollback" ]; then
		valid_rollback || fail "installation backup has an unsafe entry"
		if [ -e "$rollback/ready" ]; then
			restore_previous || fail "could not restore the previous installation; rerun install.sh"
		else
			rm -rf "$rollback" || fail "could not remove an incomplete installation backup"
		fi
	fi
	for file in $files; do
		name=${file##*/}
		directory=$prefix/${file%/*}
		if [ -d "$directory" ]; then
			rm -rf "$directory/.$name.new" || fail "could not remove the staged $name"
		fi
	done
	if [ -d "$share" ]; then
		rm -rf "$share/floor.new" || fail "could not remove the staged Floor"
		if [ -e "$share/floor" ] || [ -L "$share/floor" ]; then
			rm -rf "$share/floor.old" || fail "could not remove the old Floor backup"
		elif [ -e "$share/floor.old" ] || [ -L "$share/floor.old" ]; then
			mv "$share/floor.old" "$share/floor" || fail "could not restore the old Floor"
		fi
	fi
	for file in $files; do
		name=${file##*/}
		installed=$prefix/$file
		if [ -d "$installed" ] || {
			[ -e "$installed" ] && [ ! -f "$installed" ] && [ ! -L "$installed" ]
		}; then
			case $file in
			bin/*) kind=binary ;;
			*) kind=notice ;;
			esac
			fail "existing $name path is not a $kind file"
		fi
	done

	if [ -z "$from" ]; then
		command -v curl >/dev/null 2>&1 || fail "curl is required to download the release; install it and run this again"
	fi
	if command -v sha256sum >/dev/null 2>&1; then
		sha256() { sha256sum "$1"; }
	elif command -v shasum >/dev/null 2>&1; then
		sha256() { shasum -a 256 "$1"; }
	else
		fail "sha256sum or shasum is required to verify the download; install one and run this again"
	fi
	command -v tar >/dev/null 2>&1 || fail "tar is required to unpack the release; install it and run this again"

	if [ -n "$from" ]; then
		base=$from
	elif [ -n "${DUDES_INSTALL_BASE_URL:-}" ]; then
		base=${DUDES_INSTALL_BASE_URL%/}
		# Loopback http is for testing; a redirect could take it anywhere, so none is followed.
		case $base in
		*@*) fail "DUDES_INSTALL_BASE_URL is refused: it carries credentials" ;;
		https://*) proto='=https' redirects=10 ;;
		http://127.0.0.1 | http://127.0.0.1[:/]* | http://localhost | http://localhost[:/]*) proto='=http' redirects=0 ;;
		*) fail "DUDES_INSTALL_BASE_URL must be https, or http on 127.0.0.1 or localhost: $base" ;;
		esac
	elif [ -n "$version" ]; then
		base=$repository/releases/download/v$version
		proto='=https' redirects=10
	else
		base=$repository/releases/latest/download
		proto='=https' redirects=10
	fi

	tmp=$(mktemp -d "${TMPDIR:-/tmp}/dudes-install.XXXXXX") || fail "could not create a temporary directory"
	swapping=no
	cleanup() {
		status=$?
		trap - EXIT
		if [ "$swapping" = yes ] && [ -e "$rollback/ready" ]; then
			restore_previous || {
				printf 'dudes install: recovery incomplete; rerun install.sh\n' >&2
				status=1
			}
		fi
		for file in $files; do
			name=${file##*/}
			rm -rf "$prefix/${file%/*}/.$name.new" || status=1
		done
		rm -rf "$share/floor.new" "$tmp" || status=1
		if [ -n "$rollback" ] && [ ! -e "$rollback/ready" ]; then
			rm -rf "$rollback" || status=1
		fi
		exit "$status"
	}
	trap cleanup EXIT
	trap 'exit 1' HUP INT TERM

	fetch SHA256SUMS "$tmp/SHA256SUMS"
	suffix=-$target.tar.gz
	if [ -n "$version" ]; then
		entry=$(awk -v name="dudes-$version$suffix" '$2 == name' "$tmp/SHA256SUMS")
	else
		entry=$(awk -v suffix="$suffix" \
			'index($2, "dudes-") == 1 && substr($2, length($2) - length(suffix) + 1) == suffix' \
			"$tmp/SHA256SUMS")
	fi
	case $entry in
	'') fail "$base/SHA256SUMS lists no release for $target${version:+ at version $version}; nothing was installed" ;;
	*'
'*) fail "$base/SHA256SUMS lists more than one release for $target; pass --version" ;;
	esac
	# The archive name becomes a path, so the line must be exactly a hash and a name built from a
	# valid version and this target, and the name used is the one rebuilt from those parts.
	malformed="$base/SHA256SUMS has a malformed line for $target; nothing was installed"
	[ "$(printf '%s\n' "$entry" | awk '{ print NF }')" = 2 ] || fail "$malformed"
	expected=${entry%% *}
	listed=${entry##* }
	if [ -z "$version" ]; then
		version=${listed#dudes-}
		version=${version%"$suffix"}
		is_version "$version" || fail "$malformed"
	fi
	archive=dudes-$version$suffix
	[ "$listed" = "$archive" ] || fail "$malformed"
	case $expected in
	*[!0-9a-f]*) fail "$malformed" ;;
	esac
	[ ${#expected} -eq 64 ] || fail "$malformed"

	fetch "$archive" "$tmp/$archive"
	actual=$(sha256 "$tmp/$archive") || fail "could not hash $archive"
	actual=${actual%% *}
	[ "$actual" = "$expected" ] || fail "checksum mismatch for $archive: SHA256SUMS lists $expected, the download is $actual; nothing was installed"

	mkdir "$tmp/unpacked"
	tar -xzf "$tmp/$archive" -C "$tmp/unpacked" || fail "could not unpack $archive; it is damaged or incomplete, and nothing was installed"
	release=$tmp/unpacked/dudes-$version-$target
	for file in $files floor/release-manifest.json; do
		case $file in
		floor/*) name=$file ;;
		*) name=${file##*/} ;;
		esac
		[ -f "$release/$name" ] || fail "$archive has no $name; nothing was installed"
	done

	mkdir -p "$bin" "$share" || fail "could not create $bin and $share; choose a writable --prefix"
	for file in $files; do
		name=${file##*/}
		staged=$prefix/${file%/*}/.$name.new
		cp "$release/$name" "$staged"
		case $file in
		bin/*) chmod 755 "$staged" ;;
		*) chmod 644 "$staged" ;;
		esac
	done
	cp -R "$release/floor" "$share/floor.new"
	had_floor=no
	if [ -e "$share/floor" ] || [ -L "$share/floor" ]; then
		had_floor=yes
	fi
	rollback=$(mktemp -d "$share/.install-rollback.XXXXXX") || fail "could not create an installation backup"
	if [ "$had_floor" = no ]; then
		: > "$rollback/floor.was-absent"
	else
		: > "$rollback/floor.was-present"
	fi
	for file in $files; do
		name=${file##*/}
		installed=$prefix/$file
		if [ -e "$installed" ] || [ -L "$installed" ]; then
			cp -RPp "$installed" "$rollback/$name"
		else
			: > "$rollback/absent.$name"
		fi
	done
	: > "$rollback/ready"
	swapping=yes
	for file in $files; do
		name=${file##*/}
		mv -f "$prefix/${file%/*}/.$name.new" "$prefix/$file"
	done
	if [ "$had_floor" = yes ]; then
		mv "$share/floor" "$share/floor.old"
	fi
	mv "$share/floor.new" "$share/floor"
	rm -f "$rollback/ready"
	swapping=no
	rm -rf "$rollback" "$share/floor.old"

	say "Installed Dudes $version for $target:"
	say "  duded, dudes and dudes-hook in $bin"
	say "  the Floor in $share/floor"
	say "  the licence notices in $share"
	launcher=
	add_launcher 2>/dev/null || true
	[ -z "$launcher" ] || say "  the Dudes launcher in $launcher"
	run=dudes
	case ":${PATH:-}:" in
	*":$bin:"*) say "$bin is on your PATH." ;;
	*)
		say "$bin is not on your PATH. Add this line to your shell profile, then open a new shell:"
		say "  export PATH=\"$bin:\$PATH\""
		run=$bin/dudes
		;;
	esac
	# `ping` only asks: it starts nothing, and it fails at once when no daemon runs. A daemon that
	# started before this install keeps serving the version it loaded, Floor included.
	daemon=$("$bin/dudes" ping 2>/dev/null </dev/null | sed -n '1s/^duded \([^ ]*\) on .*/\1/p') || daemon=
	if [ -z "$daemon" ]; then
		say "Next, start the daemon:"
		say "  $run quickstart"
	elif [ "$daemon" != "$version" ]; then
		say "A Dudes daemon is still running version $daemon. Run \`$run stop\`, then \`$run start\`, to use $version."
	fi
}

main "$@"
