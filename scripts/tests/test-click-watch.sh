#!/bin/bash
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
mkdir -p "${tmp}/bin" "${tmp}/home"
log="/Users/test/Library/Logs/Grinch/Grinch_test.log"

cat > "${tmp}/bin/pgrep" <<'EOF'
#!/bin/bash
echo 4242
EOF
cat > "${tmp}/bin/lsof" <<EOF
#!/bin/bash
echo "Grinch 4242 user 9w REG 1,1 0 1 ${log}"
EOF
cat > "${tmp}/bin/plutil" <<'EOF'
#!/bin/bash
echo 7
EOF
cat > "${tmp}/bin/python3" <<EOF
#!/bin/bash
printf '%s\n' "\$@" > "${tmp}/python-args"
EOF
cat > "${tmp}/bin/tail" <<'EOF'
#!/bin/bash
exit 0
EOF
cat > "${tmp}/bin/osascript" <<'EOF'
#!/bin/bash
echo 0
EOF
chmod +x "${tmp}/bin/"*

output="$(PATH="${tmp}/bin:/usr/bin:/bin" HOME="${tmp}/home" bash "${repo}/scripts/click-watch.sh")"
grep -F "watching: ${log}" <<<"${output}" >/dev/null
grep -Fx -- "--log" "${tmp}/python-args" >/dev/null
grep -Fx -- "${log}" "${tmp}/python-args" >/dev/null

echo "click-watch shell smoke test passed"
