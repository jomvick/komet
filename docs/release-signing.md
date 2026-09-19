# Release signing

Every release publishes `manifest.json` (the sha256 of each artifact) and
`manifest.json.sig`, an Ed25519 signature over the exact manifest bytes.
The self-updater (`crates/update`) and `install.sh` install a file only when
the manifest signature is valid for the release public key and the file's
checksum matches.

## One-time setup (repository owner)

1. Generate the key pair on a trusted machine (OpenSSL 3):

   ```bash
   openssl genpkey -algorithm ed25519 -out komet-release-signing-key.pem
   openssl pkey -in komet-release-signing-key.pem -pubout -outform DER \
     | tail -c 32 | od -An -tx1 | tr -d ' \n'; echo
   ```

   The second command prints the public key as 64 hex characters.

2. In the repository settings, under Secrets and variables, then Actions:
   - Secret `RELEASE_SIGNING_KEY`: the full contents of
     `komet-release-signing-key.pem`.
   - Variable `KOMET_RELEASE_PUBLIC_KEY`: the 64 hex characters.

3. Put the same public key in `install.sh` (`RELEASE_PUBLIC_KEY`).

4. Store the private key offline (password manager or hardware token) and
   delete the local file.

Tagged builds fail if the variable is missing, and the publish job fails if
the secret is missing, so a release can never ship unsigned.

## What each part does

- Build jobs compile `KOMET_RELEASE_PUBLIC_KEY` into the binaries.
- The publish job signs `manifest.json` with
  `openssl pkeyutl -sign -rawin`, verifies the signature against the public
  key, then uploads both files.
- `KOMET_RELEASE_REPO` changes where updates are downloaded from, not which
  key is trusted. A fork has to build with its own key.

## Rotating the key

Binaries only trust the key they were built with, so one release has to be
signed with the old key (so current installs accept it) while being built
with the new public key (so it trusts the releases after it). To rotate:

1. Generate the new key pair.
2. Rotation release:
   - Variable `KOMET_RELEASE_PUBLIC_KEY`: the new public key.
   - Variable `KOMET_RELEASE_SIGNING_PUBLIC_KEY`: the old public key.
   - Secret `RELEASE_SIGNING_KEY`: still the old private key.
   - `install.sh` keeps the old public key, because this release is signed
     with the old key.
   - Tag and publish the release.
3. Following releases:
   - Secret `RELEASE_SIGNING_KEY`: the new private key.
   - Delete the variable `KOMET_RELEASE_SIGNING_PUBLIC_KEY`.
   - Put the new public key in `install.sh`, in the same commit as the first
     release signed with the new key.

The publish job checks the signature against `KOMET_RELEASE_SIGNING_PUBLIC_KEY`
when it is set, otherwise against `KOMET_RELEASE_PUBLIC_KEY`, so a mismatched
secret fails the release before anything is uploaded.

If the private key leaks, installs built with it will accept anything signed
with it: publish a release with a new key as soon as possible and tell users
to reinstall with `install.sh`.

## Upgrading from releases before signing

Installed versions that predate signing do not check signatures and keep
updating as before. Once a user runs a signed build, only signed releases are
accepted.
