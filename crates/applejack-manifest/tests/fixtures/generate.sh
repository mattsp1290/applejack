#!/bin/sh
# Derives every invalid fixture from valid.toml so the set is reproducible.
# Usage: tests/fixtures/generate.sh (from any directory). Commit the output.
set -eu

dir=$(cd "$(dirname "$0")" && pwd)
valid="$dir/valid.toml"

# drop_field TABLE FIELD: print valid.toml without the "FIELD = ..." line of
# the [[TABLE]] block.
drop_field() {
    awk -v table="$1" -v field="$2" '
        /^\[\[/ { current = $0 }
        current == "[[" table "]]" && index($0, field " = ") == 1 { next }
        { print }
    ' "$valid"
}

rm -f "$dir"/missing-*.toml "$dir"/bad-*.toml "$dir"/duplicate-id.toml

for field in id version arch min_macos url sha256 components notarization_submission_id; do
    drop_field engine "$field" > "$dir/missing-engine-$field.toml"
done
# tool.arch is optional, so no missing-tool-arch fixture exists.
for field in id version url sha256 runs_on; do
    drop_field tool "$field" > "$dir/missing-tool-$field.toml"
done
for field in id version url sha256 arch runs_in ships_in_bundle; do
    drop_field probe "$field" > "$dir/missing-probe-$field.toml"
done

# 63-character digests.
sed -E 's/^(sha256 = "[0-9a-f]{63})[0-9a-f]"$/\1"/' "$valid" > "$dir/bad-sha256.toml"

# Plain http.
sed -E 's#^url = "https://#url = "http://#' "$valid" > "$dir/bad-url.toml"

# The [[tool]] block repeated verbatim.
{
    cat "$valid"
    printf '\n'
    awk '/^\[\[tool\]\]/ { keep = 1 } /^\[\[probe\]\]/ { keep = 0 } keep' "$valid"
} > "$dir/duplicate-id.toml"
