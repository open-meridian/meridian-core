#!/bin/sh
# `ci-sdk.sh [REF]`: which meridian-python ref interop reads (v31a item 11).
#
# REF, where given and not empty, is a run's own `sdk_ref` input (a run by
# hand); otherwise the release tools/ci-sdk.txt pins. Says which, and why, and
# writes `ref=<it>` to $GITHUB_OUTPUT where there is one. A ref that is not a
# release tag (vMAJOR.MINOR.PATCH) is said as a ::warning:: on every run that
# reads it, so a branch pin left on main is never silent. Exit 1 where the ref
# holds anything but a branch's or tag's characters, or the pin file names
# none. Before v31a a pull request read the SDK's branch of its own name where
# one existed, and main otherwise, so an SDK merge could turn core's CI red.
set -eu
here=$(dirname "$0")
given=${1:-}
if [ -n "$given" ]; then
	ref=$given
	why="this run's sdk_ref input"
else
	ref=$(sed -e 's/#.*//' -e 's/[[:space:]]//g' "$here/ci-sdk.txt" | sed '/^$/d' | head -n 1)
	why="tools/ci-sdk.txt"
fi
case "$ref" in
"" | *[!A-Za-z0-9._/-]*)
	echo "::error::ci-sdk: '$ref' (from $why) is not a branch or tag of meridian-python"
	exit 1
	;;
esac
if expr "$ref" : 'v[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*$' >/dev/null; then
	echo "interop reads meridian-python@$ref, the release $why names"
else
	echo "::warning title=interop reads no release::interop reads meridian-python@$ref ($why), not a release tag"
fi
[ -z "${GITHUB_OUTPUT:-}" ] || echo "ref=$ref" >>"$GITHUB_OUTPUT"
