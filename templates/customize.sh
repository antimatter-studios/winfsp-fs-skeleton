#!/usr/bin/env bash
# customize.sh -- materialise the templates under templates/ into a fresh
# consumer repo, with all the placeholders substituted for your project's
# values.
#
# The substitution table is the one in templates/README.md ("Substitutions"
# section). When that table changes, update the TOKEN lookups below to
# match -- README is canonical.
#
# This script is pragmatic, not Bash-perfectionist: long-form flags only,
# minimal validation, no dependencies beyond `sed`, `cp`, `find`, and
# (optionally) `uuidgen`.

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: customize.sh --target DIR [options]

Required:
  --target DIR              Destination consumer repo. Must exist and be
                            empty (other than an optional .git/ dir).
  --name NAME               Crate / package name           (e.g. ext4-win-driver)
  --fs-name NAME            FS short name                  (e.g. ext4)
  --service-name NAME       SCM service name               (e.g. ExtFsWatcher)
  --launcher-class NAME     WinFsp.Launcher service class  (e.g. ext4-mount)
  --file-extension EXT      File extension verb (no dot)   (e.g. img)
  --publisher-id ID         winget publisher id            (e.g. AntimatterStudios)
  --publisher-name NAME     Display publisher name         (e.g. "Antimatter Studios")
  --manufacturer NAME       MSI Manufacturer attribute     (e.g. "Your Name")
  --winfsp-version VER      WinFsp redist pin              (e.g. 2.1.25156)
  --winfsp-sha256 HEX       WinFsp redist SHA256 pin

Optional:
  --verb-display-name STR   "Mount as <fs>" string. Defaults to
                            "Mount as <fs-name>".
  --github-repo SLUG        GitHub owner/repo. Defaults to
                            "<publisher-id>/<name>".
  --winget-id ID            winget package id. Defaults to
                            "<publisher-id>.<name>".
  --upgrade-code-msi GUID   MSI UpgradeCode. Generated with uuidgen if absent.
  --upgrade-code-bundle GUID
                            Bundle UpgradeCode. Generated with uuidgen if absent.

  -h, --help                This help text.

Reference: templates/README.md (the "Substitutions" table is the canonical
source for which tokens get replaced).
EOF
}

# ---- arg parsing ------------------------------------------------------------

NAME=""
FS_NAME=""
SERVICE_NAME=""
LAUNCHER_CLASS=""
FILE_EXTENSION=""
PUBLISHER_ID=""
PUBLISHER_NAME=""
MANUFACTURER=""
WINFSP_VERSION=""
WINFSP_SHA256=""
TARGET=""
VERB_DISPLAY_NAME=""
GITHUB_REPO=""
WINGET_ID=""
UPGRADE_CODE_MSI=""
UPGRADE_CODE_BUNDLE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name) NAME="$2"; shift 2 ;;
    --fs-name) FS_NAME="$2"; shift 2 ;;
    --service-name) SERVICE_NAME="$2"; shift 2 ;;
    --launcher-class) LAUNCHER_CLASS="$2"; shift 2 ;;
    --file-extension) FILE_EXTENSION="$2"; shift 2 ;;
    --publisher-id) PUBLISHER_ID="$2"; shift 2 ;;
    --publisher-name) PUBLISHER_NAME="$2"; shift 2 ;;
    --manufacturer) MANUFACTURER="$2"; shift 2 ;;
    --winfsp-version) WINFSP_VERSION="$2"; shift 2 ;;
    --winfsp-sha256) WINFSP_SHA256="$2"; shift 2 ;;
    --target) TARGET="$2"; shift 2 ;;
    --verb-display-name) VERB_DISPLAY_NAME="$2"; shift 2 ;;
    --github-repo) GITHUB_REPO="$2"; shift 2 ;;
    --winget-id) WINGET_ID="$2"; shift 2 ;;
    --upgrade-code-msi) UPGRADE_CODE_MSI="$2"; shift 2 ;;
    --upgrade-code-bundle) UPGRADE_CODE_BUNDLE="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown flag: $1" >&2; usage >&2; exit 2 ;;
  esac
done

require() {
  local name="$1" val="$2"
  if [[ -z "$val" ]]; then
    echo "missing required flag: $name" >&2
    exit 2
  fi
}

require --target "$TARGET"
require --name "$NAME"
require --fs-name "$FS_NAME"
require --service-name "$SERVICE_NAME"
require --launcher-class "$LAUNCHER_CLASS"
require --file-extension "$FILE_EXTENSION"
require --publisher-id "$PUBLISHER_ID"
require --publisher-name "$PUBLISHER_NAME"
require --manufacturer "$MANUFACTURER"
require --winfsp-version "$WINFSP_VERSION"
require --winfsp-sha256 "$WINFSP_SHA256"

# Defaults derived from required flags.
[[ -z "$VERB_DISPLAY_NAME" ]] && VERB_DISPLAY_NAME="Mount as ${FS_NAME}"
[[ -z "$GITHUB_REPO" ]] && GITHUB_REPO="${PUBLISHER_ID}/${NAME}"
[[ -z "$WINGET_ID" ]] && WINGET_ID="${PUBLISHER_ID}.${NAME}"

gen_guid() {
  if command -v uuidgen >/dev/null 2>&1; then
    uuidgen | tr '[:upper:]' '[:lower:]'
  else
    echo "uuidgen not found and --upgrade-code-* not provided" >&2
    exit 2
  fi
}

[[ -z "$UPGRADE_CODE_MSI" ]] && UPGRADE_CODE_MSI="$(gen_guid)"
[[ -z "$UPGRADE_CODE_BUNDLE" ]] && UPGRADE_CODE_BUNDLE="$(gen_guid)"

# ---- target sanity ----------------------------------------------------------

if [[ ! -d "$TARGET" ]]; then
  echo "--target $TARGET does not exist (create it first)" >&2
  exit 2
fi

# Allow only `.git/` as a pre-existing entry. Empty otherwise.
shopt -s nullglob dotglob
extras=()
for entry in "$TARGET"/*; do
  base="$(basename "$entry")"
  if [[ "$base" != ".git" ]]; then
    extras+=("$base")
  fi
done
shopt -u nullglob dotglob
if (( ${#extras[@]} > 0 )); then
  echo "--target $TARGET is not empty (contains: ${extras[*]})" >&2
  echo "Refusing to overwrite. Move or clear the directory first." >&2
  exit 2
fi

# ---- copy --------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$TARGET/installer" "$TARGET/.github/workflows" "$TARGET/winget"
cp -R "$SCRIPT_DIR/installer/." "$TARGET/installer/"
cp    "$SCRIPT_DIR/release.yml"  "$TARGET/.github/workflows/release.yml"
cp -R "$SCRIPT_DIR/winget/."     "$TARGET/winget/"

# Rename Mount-Fs.ps1 to follow the consumer's filesystem short name (e.g.
# Mount-Ext4.ps1). This matches the substitution table in README.md.
fs_title="$(printf '%s' "$FS_NAME" | awk '{print toupper(substr($0,1,1)) substr($0,2)}')"
if [[ -f "$TARGET/installer/Mount-Fs.ps1" ]]; then
  mv "$TARGET/installer/Mount-Fs.ps1" "$TARGET/installer/Mount-${fs_title}.ps1"
fi

# ---- substitute --------------------------------------------------------------

# Reference values that appear in the shipped templates today (extracted from
# the ext4-win-driver consumer). Mapping these to consumer-supplied
# replacements is what this script exists to do.
#
# The list below mirrors the "Substitutions" table in README.md. When the
# table changes, update both -- README stays canonical.

#
# Order matters: compound / longer forms come first so they substitute
# atomically before any of their substrings get rewritten by a later rule.
# E.g. `antimatter-studios/ext4-win-driver` must be replaced before
# `ext4-win-driver` alone, or the compound rule won't match anything.
declare -a REPLACEMENTS=(
  # winget package id (compound: <publisher>.<name>)
  "AntimatterStudios.ext4-win-driver|${WINGET_ID}"
  # GitHub repo slug (compound: <org>/<repo>)
  "antimatter-studios/ext4-win-driver|${GITHUB_REPO}"
  # Bare GitHub org slug (PublisherUrl). The org id used in URLs is
  # lowercased+hyphenated; use the publisher id if the consumer hasn't
  # told us otherwise -- close enough for the templates' purposes.
  "antimatter-studios|${PUBLISHER_ID}"
  # crate / package name
  "ext4-win-driver|${NAME}"
  # SCM service name
  "ExtFsWatcher|${SERVICE_NAME}"
  # WinFsp.Launcher class
  "ext4-mount|${LAUNCHER_CLASS}"
  # verb display name (compound: contains the fs-name substring)
  "Mount as ext4|${VERB_DISPLAY_NAME}"
  # fs short name
  "ext4|${FS_NAME}"
  # file extension verb
  ".img|.${FILE_EXTENSION}"
  # manufacturer / publisher (display strings)
  "Antimatter Studios|${PUBLISHER_NAME}"
  "AntimatterStudios|${PUBLISHER_ID}"
  "Chris Thomas|${MANUFACTURER}"
  # MSI / Bundle UpgradeCodes (reference values from the shipped templates;
  # consumers MUST get fresh GUIDs -- regenerated above if not supplied).
  "d6f3a7c2-9b14-4e7a-9b23-7c5f1f4a8e91|${UPGRADE_CODE_MSI}"
  "b6d4e8a1-3f29-4c7d-9e22-1a8c5d6f7e34|${UPGRADE_CODE_BUNDLE}"
  # WinFsp redist version + sha256 pins. The version literal also catches
  # the bundled MSI filename (winfsp-<version>.msi) in build.ps1.
  "2.1.25156|${WINFSP_VERSION}"
  "073a70e00f77423e34bed98b86e600def93393ba5822204fac57a29324db9f7a|${WINFSP_SHA256}"
)

# Build a single sed program. We use `|` as the s/// delimiter because none of
# the replacement values are expected to contain `|`. Replacement strings are
# escaped for sed metachars (& and \) and the chosen delimiter.
sed_escape() {
  printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'
}

SED_PROG=""
for r in "${REPLACEMENTS[@]}"; do
  from="${r%%|*}"
  to="${r#*|}"
  from_esc="$(sed_escape "$from")"
  to_esc="$(sed_escape "$to")"
  SED_PROG+="s|${from_esc}|${to_esc}|g;"
done

# Apply to every file in the copied tree. Skip any binary files (none are
# expected, but the safety belt is cheap).
while IFS= read -r -d '' f; do
  if file "$f" 2>/dev/null | grep -q "text\|XML\|empty\|JSON\|ASCII"; then
    # macOS and Linux sed take different in-place flags. -i '' on BSD,
    # -i alone on GNU. The portable workaround is a tempfile.
    tmp="$(mktemp)"
    sed "$SED_PROG" "$f" >"$tmp"
    cat "$tmp" >"$f"
    rm -f "$tmp"
  fi
done < <(find "$TARGET/installer" "$TARGET/.github" "$TARGET/winget" -type f -print0)

# ---- summary ----------------------------------------------------------------

cat <<EOF
customized templates -> $TARGET

  installer/                 (Bundle.wxs, Product.wxs, build.ps1, Mount-${fs_title}.ps1, update-winfsp-pin.sh)
  .github/workflows/release.yml
  winget/                    (installer.yaml, locale.en-US.yaml, version.yaml)

substitutions applied:
  crate name           = ${NAME}
  fs short name        = ${FS_NAME}
  service name         = ${SERVICE_NAME}
  launcher class       = ${LAUNCHER_CLASS}
  file extension       = .${FILE_EXTENSION}
  verb display name    = ${VERB_DISPLAY_NAME}
  manufacturer         = ${MANUFACTURER}
  publisher name       = ${PUBLISHER_NAME}
  publisher id         = ${PUBLISHER_ID}
  github repo          = ${GITHUB_REPO}
  winget id            = ${WINGET_ID}
  winfsp version       = ${WINFSP_VERSION}
  winfsp sha256        = ${WINFSP_SHA256}
  MSI UpgradeCode      = ${UPGRADE_CODE_MSI}
  Bundle UpgradeCode   = ${UPGRADE_CODE_BUNDLE}

next steps:
  - cd $TARGET
  - review the customized files (a diff against the skeleton's templates/
    is the cleanest way to spot anything that needs further touching)
  - wire up your Cargo.toml + src/main.rs against winfsp-fs-skeleton
EOF
