# winget

`winget install splaude` instead of "download an exe and click through
SmartScreen twice".

The three files beside this one are the manifest winget wants, with two things
left blank: the version, and the SHA-256 of the `.exe` for that version. The
release workflow fills both in and attaches the result to the release, so
submitting a new version is copying three finished files rather than editing
YAML by hand at the moment you least want to.

## Why this is reachable and Homebrew is not

winget does not require code signing. Its own documentation says installers must
be MSIX, MSI, APPX or `.exe`, and says nothing about a certificate — so an
unsigned `splaude.exe` is eligible today, at no cost and with no Apple or
Microsoft account involved.

Homebrew's cask tap is the opposite: it stopped accepting unsigned binaries some
time ago. A `brew install --cask splaude` is therefore gated behind the $100/year
Apple Developer Program, which is a spending decision rather than an engineering
one — it sits in [Triage](../../ROADMAP.md#triage) until somebody makes it.

SmartScreen is unaffected by any of this. winget installs the same unsigned
binary, and the first run still shows the reputation warning; what changes is
that finding and fetching it is one command. Clearing the warning needs a
certificate, which is the same Triage row.

## Submitting a version

1. Cut the release. The workflow attaches `winget-<version>.zip`, holding the
   three manifest files with the version and hash already in them.
2. Check the hash is the one you are shipping:

   ```sh
   sha256sum splaude-<version>-windows-x86_64.exe
   ```

3. Validate and submit:

   ```sh
   winget validate --manifest .
   winget install --manifest .        # installs it locally, from the manifest
   ```

   Then open a pull request adding the three files to
   [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) under
   `manifests/b/bygelo/splaude/<version>/`. An automated validation pipeline
   runs on the pull request; a manifest that fails `winget validate` locally
   will fail there too, which is why step 3 comes before the pull request.

## What the manifest deliberately does not do

- **No silent-install switches.** splaude is a single portable `.exe`, not an
  installer; there is nothing to make quiet.
- **No `InstallerType: portable` shortcuts to a Start Menu entry.** winget's
  portable handling puts the binary on `PATH`, which is what makes
  `splaude --settings`, `splaude --toggle` and `splaude --check` work from
  anywhere, and that is the whole reason to want it on `PATH`.
- **No auto-update hook.** splaude checks for updates and says so; it does not
  install them. `winget upgrade splaude` is the honest way to take one, and it
  is the reason a manifest is worth having at all.
