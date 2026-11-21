#!/usr/bin/env bash
# Sets the six GitHub secrets the "Signed Mac app" workflow needs
# (.github/workflows/mac-signed.yml), after checking the certificate.
#
#   bash packaging/macos/set-secrets.sh path/to/DeveloperID.p12 [--repo owner/name]
#
# Runs in Terminal on a Mac, or in Git Bash on Windows. Needs the GitHub CLI
# (gh), signed in as an account that owns the repository, and openssl.
#
# Nothing secret is put on a command line or written to disk: passwords are
# asked for without echoing, and every value goes to gh on its standard input.
# The secrets go to the repository itself: organisation secrets don't reach a
# personal-account repository (docs/mac-signing.md).

set -euo pipefail

REPO="pinnet/AtticArc"
P12=""
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO="${2:?--repo needs owner/name}"; shift 2 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) P12="$1"; shift ;;
  esac
done

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32mok\033[0m  %s\n' "$*"; }
bad()  { printf '  \033[31m!!\033[0m  %s\n' "$*" >&2; exit 1; }
ask_secret() { # prompt, variable name
  local v; printf '  %s: ' "$1" >&2; IFS= read -rs v; printf '\n' >&2; printf -v "$2" '%s' "$v"
}

# Git Bash: a Windows path (C:\Users\... or C:/Users/...) becomes /c/Users/...
if [ -n "$P12" ] && command -v cygpath >/dev/null 2>&1 && [[ "$P12" =~ ^[A-Za-z]:[\\/] ]]; then
  P12=$(cygpath -u "$P12")
fi

say "1. Tools"
command -v gh >/dev/null || bad "gh isn't installed: https://cli.github.com (Mac: brew install gh; Windows: winget install GitHub.cli)"
command -v openssl >/dev/null || bad "openssl isn't installed (it comes with macOS and Git Bash)"
ok "gh $(gh --version | head -1 | awk '{print $3}'), $(openssl version | awk '{print $1, $2}')"
gh auth status >/dev/null 2>&1 || bad "gh isn't signed in: run  gh auth login  (GitHub.com, HTTPS, log in with a browser), then run this again"
who=$(gh api user --jq .login)
ok "signed in to GitHub as $who"
perm=$(gh api "repos/$REPO" --jq '.permissions.admin' 2>/dev/null) || bad "can't see $REPO as $who: wrong account? (gh auth switch)"
[ "$perm" = "true" ] || bad "$who isn't an admin of $REPO, so it can't set its secrets"
ok "$who can set secrets on $REPO"

say "2. The certificate"
[ -n "$P12" ] || bad "give the .p12 file: bash $0 path/to/DeveloperID.p12"
if [ ! -f "$P12" ]; then
  if [[ "$P12" =~ ^[A-Za-z]:[^\\/] ]]; then
    bad "the path lost its backslashes on the way in ($P12). In Git Bash put it in single quotes: 'C:\Users\danny\Desktop\DeveloperID.p12', or use /c/Users/danny/Desktop/DeveloperID.p12"
  fi
  bad "$P12 isn't there. If it's in the OneDrive Personal Vault, copy it out first (a locked vault hides its files); a path with spaces needs quotes"
fi
case "$P12" in *.p12|*.pfx) ;; *) bad "$P12 isn't a .p12: export one from Keychain Access (My Certificates → right-click the Developer ID Application certificate → Export, format .p12). A .cer alone has no private key and can't sign." ;; esac
ask_secret "Password for $(basename "$P12")" P12_PASS
export P12_PASS
# Keychain's .p12 files use an old cipher: OpenSSL 3 needs -legacy, macOS's LibreSSL refuses it.
p12() { openssl pkcs12 -in "$P12" -passin env:P12_PASS "$@" 2>/dev/null || openssl pkcs12 -legacy -in "$P12" -passin env:P12_PASS "$@" 2>/dev/null; }
cert=$(p12 -nokeys -clcerts) || bad "can't open $P12 with that password"
# -clcerts gives only a certificate with its key beside it.
if ! printf '%s\n' "$cert" | grep -q "BEGIN CERTIFICATE"; then
  p12 -nokeys | grep -q "BEGIN CERTIFICATE" \
    && bad "the .p12 has the certificate but not its private key: export it again from the Mac whose Keychain made the certificate (the key shows under it, with a disclosure triangle)" \
    || bad "there's no certificate in $P12"
fi
subject=$(printf '%s\n' "$cert" | openssl x509 -noout -subject)
case "$subject" in
  *"Developer ID Application"*) ok "Developer ID Application certificate" ;;
  *"Apple Development"*|*"Mac Developer"*|*"Apple Distribution"*|*"3rd Party Mac Developer"*|*"Developer ID Installer"*)
    bad "this is the wrong kind of certificate ($subject). The workflow needs 'Developer ID Application' (Xcode → Settings → Accounts → Manage Certificates → + → Developer ID Application)" ;;
  *) bad "not an Apple Developer ID certificate: $subject" ;;
esac
printf '%s\n' "$cert" | openssl x509 -noout -checkend 0 >/dev/null || bad "the certificate has expired ($(printf '%s\n' "$cert" | openssl x509 -noout -enddate))"
ok "valid until $(printf '%s\n' "$cert" | openssl x509 -noout -enddate | cut -d= -f2)"
p12 -nocerts -nodes | grep -q "PRIVATE KEY" || bad "the .p12 has no private key: export it again from the Mac whose Keychain made the certificate, ticking the key too"
ok "has its private key"
TEAM_ID=$(printf '%s' "$subject" | sed -nE 's/.*OU *= *([A-Z0-9]{10}).*/\1/p')
[ -n "$TEAM_ID" ] || TEAM_ID=$(printf '%s' "$subject" | sed -nE 's/.*\(([A-Z0-9]{10})\).*/\1/p')
[ -n "$TEAM_ID" ] || bad "can't find the Team ID in the certificate: $subject"
ok "Team ID $TEAM_ID (from the certificate)"

say "3. Apple ID and app-specific password (for notarising)"
printf '  Apple ID email: '; IFS= read -r APPLE_ID
case "$APPLE_ID" in *@*.*) ;; *) bad "'$APPLE_ID' doesn't look like an email" ;; esac
echo "  An app-specific password, not your Apple ID password: account.apple.com →"
echo "  Sign-In and Security → App-Specific Passwords → +  (looks like abcd-efgh-ijkl-mnop)"
ask_secret "App-specific password" APP_PASS
[[ "$APP_PASS" =~ ^[a-z]{4}-[a-z]{4}-[a-z]{4}-[a-z]{4}$ ]] || bad "that isn't in the form abcd-efgh-ijkl-mnop"
ok "looks right (Apple checks it when the workflow notarises)"

say "4. Setting the secrets on $REPO"
existing=$(gh secret list --repo "$REPO" --json name --jq '.[].name' | tr '\n' ' ')
[ -z "$existing" ] || echo "  Already there (will be replaced): $existing"
printf '  Set all six now? [y/N] '; read -r yn
[ "$yn" = "y" ] || [ "$yn" = "Y" ] || { echo "  Nothing set."; exit 0; }
KEYCHAIN_PASSWORD=$(openssl rand -base64 24)   # only for the runner's throwaway keychain
put() { printf '%s' "$2" | gh secret set "$1" --repo "$REPO" >/dev/null || bad "couldn't set $1 (the ones above it were set; run this again)"; ok "$1"; }
put MACOS_CERT_P12 "$(base64 < "$P12" | tr -d '\r\n')"
put MACOS_CERT_PASSWORD "$P12_PASS"
put KEYCHAIN_PASSWORD "$KEYCHAIN_PASSWORD"
put APPLE_ID "$APPLE_ID"
put APPLE_TEAM_ID "$TEAM_ID"
put APPLE_APP_PASSWORD "$APP_PASS"
unset P12_PASS APP_PASS KEYCHAIN_PASSWORD

say "5. Next"
if gh workflow view mac-signed.yml --repo "$REPO" >/dev/null 2>&1; then
  printf '  The "Signed Mac app" workflow is on GitHub. Run it now? [y/N] '; read -r yn
  if [ "$yn" = "y" ] || [ "$yn" = "Y" ]; then
    gh workflow run mac-signed.yml --repo "$REPO"
    sleep 5
    gh run watch --repo "$REPO" "$(gh run list --repo "$REPO" --workflow mac-signed.yml --limit 1 --json databaseId --jq '.[0].databaseId')" || true
  fi
else
  echo "  The workflow isn't on GitHub yet. Move docs/ci/mac-signed.yml to"
  echo "  .github/workflows/mac-signed.yml, commit and push, then: Actions → Signed Mac app → Run workflow."
fi
echo
echo "  Then delete $(basename "$P12") from here (it is still in your Personal Vault), and lock the vault."
