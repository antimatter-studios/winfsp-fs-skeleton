#!/usr/bin/env bash
# test-customize.sh -- customize.sh, run for a driver that shares nothing with
# the reference consumer, leaves none of the reference values behind and
# produces an installer whose files exist.
#
# Every value passed below is chosen so that no reference value (ext4,
# ext4-win-driver, ExtFsWatcher, the reference GUIDs and WinFsp pins) is a
# substring of it: anything of the reference left in the output was missed
# by a substitution, not reintroduced by one.
#
# Usage: templates/test-customize.sh   (from anywhere; exits non-zero on failure)

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fails=0
ok()   { printf 'ok   %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; fails=$((fails + 1)); }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
out="$work/out"
mkdir -p "$out"

repo="antimatter-studios/zfs-win-driver"

if bash "$HERE/customize.sh" \
    --target "$out" \
    --name zfs-win-driver \
    --fs-name zfs \
    --service-name ZettaWatcher \
    --launcher-class zfs-mount \
    --file-extension vhd \
    --publisher-id AcmeCorp \
    --publisher-name "Acme Corp" \
    --manufacturer "Test Maker" \
    --winfsp-version 9.8.7 \
    --winfsp-sha256 0000aaaa0000aaaa0000aaaa0000aaaa0000aaaa0000aaaa0000aaaa0000aaaa \
    --github-repo "$repo" \
    --upgrade-code-msi 11111111-1111-1111-1111-111111111111 \
    --upgrade-code-bundle 22222222-2222-2222-2222-222222222222 \
    >"$work/log" 2>&1; then
  ok "customize.sh exits 0"
else
  fail "customize.sh exits non-zero:"
  cat "$work/log"
fi

# 1. No reference value survives, in any spelling.
left="$(grep -rn -i -E 'ext4|ExtFs|Chris Thomas|d6f3a7c2-9b14-4e7a-9b23-7c5f1f4a8e91|b6d4e8a1-3f29-4c7d-9e22-1a8c5d6f7e34|2\.1\.25156|073a70e00f77423e34bed98b86e600def93393ba5822204fac57a29324db9f7a' "$out" || true)"
if [[ -z "$left" ]]; then
  ok "no reference value is left in the customized tree"
else
  fail "reference values are left in the customized tree:"
  printf '%s\n' "$left"
fi

# 2. Every Mount-*.ps1 the installer names is a file it can install.
missing=""
for f in $(grep -rhoE 'Mount-[A-Za-z0-9]+\.ps1' "$out/installer" | sort -u); do
  [[ -f "$out/installer/$f" ]] || missing+=" $f"
done
if [[ -z "$missing" ]]; then
  ok "every Mount-*.ps1 named by the installer exists"
else
  fail "the installer names Mount scripts that do not exist:$missing"
fi

# 3. --github-repo is written exactly as given wherever the repository is linked.
for spec in \
    "installer/Bundle.wxs|AboutUrl=\"https://github.com/$repo\"" \
    "installer/Product.wxs|Value=\"https://github.com/$repo\"" \
    "winget/locale.en-US.yaml|PackageUrl: https://github.com/$repo" \
    "winget/locale.en-US.yaml|PublisherUrl: https://github.com/${repo%%/*}"; do
  file="${spec%%|*}"
  want="${spec#*|}"
  if grep -qF -- "$want" "$out/$file"; then
    ok "$file links $want"
  else
    fail "$file does not link $want; it has: $(grep -E 'github\.com' "$out/$file" | head -3 | tr '\n' ' ')"
  fi
done

if (( fails > 0 )); then
  printf '%s: %d check(s) failed\n' "$(basename "$0")" "$fails"
  exit 1
fi
printf '%s: all checks passed\n' "$(basename "$0")"
