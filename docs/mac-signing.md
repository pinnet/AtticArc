# Signing and notarising the Mac app

`.github/workflows/mac-signed.yml` builds `AtticARC.app` for Apple Silicon (the GUI as
`atticarc`, with `atticarc-cli` beside it in `Contents/MacOS`), signs both with the Developer ID
certificate (hardened runtime, secure timestamp), puts the app in
`AtticARC-<version>-macos-arm64.dmg`, has Apple notarise the .dmg and staples the ticket to it.
A Mac then opens it without the "unidentified developer" warning, offline too.

It is the same workflow as Plugin²'s (pinnet/plugin-squared, `docs/mac-signing.md` there has
the full one-time setup: the Developer ID Application certificate, the app-specific password,
the Apple agreements), with AtticARC's names. The bundle id is `com.pinnet.atticarc`
(`packaging/macos/Info.plist`); the version comes from `Cargo.toml`, the build number from the
run.

It runs on every `v*` tag (attaching the .dmg to the release) and by hand from Actions →
**Signed Mac app** → **Run workflow** once the workflow is on `main` (GitHub only offers that
button for workflows on the default branch). Without the secrets it still builds the unsigned
.dmg and says in the run's summary which secrets are missing.

## Secrets

Six repository secrets on `pinnet/AtticArc` (secrets of the `dannyarnoldcom` organisation don't
reach a personal-account repository):

| Secret | Value |
|---|---|
| `MACOS_CERT_P12` | the Developer ID Application `.p12`, as base64 |
| `MACOS_CERT_PASSWORD` | its export password |
| `KEYCHAIN_PASSWORD` | any random password (the runner's throwaway keychain) |
| `APPLE_ID` | your Apple ID email |
| `APPLE_TEAM_ID` | the 10-character Team ID |
| `APPLE_APP_PASSWORD` | an app-specific password (account.apple.com) |

The quick way: `bash packaging/macos/set-secrets.sh path/to/DeveloperID.p12` (Terminal on a Mac,
or Git Bash on Windows; in Git Bash put a Windows path in single quotes). It checks the
certificate, takes the Team ID from it, asks for the rest without echoing, and sets all six
with `gh`. The same certificate and passwords as Plugin²'s.

## Also on every pull request

`.github/workflows/check.yml` formats, lints, tests and builds on Windows, macOS and Linux, so
a change that breaks one of the three shows at once (docs/refactor-brief.md, "The outcome").
