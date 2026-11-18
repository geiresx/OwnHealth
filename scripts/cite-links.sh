#!/usr/bin/env bash
# Check every https link that the cited data files name (the YAML, JSON and TSV files under data/:
# `cite` fields, `citation` columns, source notes). Fetch each link once and print a Markdown report.
# Exit 0 when every link answers, 1 when one is dead, and 2 when the check could not run (no links
# found, no curl).
#
#   bash scripts/cite-links.sh            # the repo's data/
#   bash scripts/cite-links.sh <dir>      # another tree, for example a copy with a broken link
#
# A link is dead when its host does not resolve or refuses the connection or the TLS handshake, or
# when it answers 404 or 410. A link that answers 401, 403, 429 or a 5xx, or does not answer within
# 30 seconds after two retries, could not be checked: many sites refuse scripts. The report lists
# those apart, and they do not count as dead. .github/workflows/cite-links.yml runs this every week.
set -uo pipefail

dir=${1:-$(cd "$(dirname "$0")/.." && pwd)/data}
[ -d "$dir" ] || { echo "cite-links: no directory $dir" >&2; exit 2; }
command -v curl >/dev/null || { echo "cite-links: curl is not installed" >&2; exit 2; }
ua='Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36'

files() { grep -rlF --include='*.yaml' --include='*.yml' --include='*.json' --include='*.tsv' -- "$1" "$dir"; }

urls=$(grep -rhoE --include='*.yaml' --include='*.yml' --include='*.json' --include='*.tsv' \
  "https://[^\"'[:space:],}<>|]+" "$dir" | sed -E 's/[.)]+$//' | sort -u)
total=$(printf '%s\n' "$urls" | grep -c '^https://')
[ "$total" -gt 0 ] || { echo "cite-links: no https links found under $dir" >&2; exit 2; }

dead='' unchecked=''
while IFS= read -r url; do
  code=$(curl -sS -L -o /dev/null -w '%{http_code}' --max-time 30 --retry 2 --retry-delay 5 \
    -A "$ua" -- "$url" 2>/dev/null)
  rc=$?
  if [ "$rc" -ne 0 ]; then
    case "$rc" in
      6 | 7 | 35 | 51 | 60) list=dead ;;       # no such host, connection refused, TLS
      *) list=unchecked ;;                      # a timeout or another transfer error
    esac
    code="no answer (curl exit $rc)"
  else
    case "$code" in
      2?? | 3??) continue ;;
      404 | 410) list=dead ;;
      *) list=unchecked ;;
    esac
  fi
  where=$(files "$url" | sed "s|^$dir/|data/|" | sort | paste -sd, - | sed 's/,/, /g')
  line="| $url | $code | $where |"
  if [ "$list" = dead ]; then dead+="$line"$'\n'; else unchecked+="$line"$'\n'; fi
done <<<"$urls"

n_dead=$(printf '%s' "$dead" | grep -c '^|')
n_unchecked=$(printf '%s' "$unchecked" | grep -c '^|')
echo "Checked $total links under data/: $n_dead dead, $n_unchecked could not be checked by a script."
if [ "$n_dead" -gt 0 ]; then
  printf '\n## Dead\n\n| Link | Answer | Used in |\n|---|---|---|\n%s' "$dead"
fi
if [ "$n_unchecked" -gt 0 ]; then
  printf '\n## Could not be checked by a script (open them in a browser)\n\n| Link | Answer | Used in |\n|---|---|---|\n%s' "$unchecked"
fi
[ "$n_dead" -eq 0 ]
