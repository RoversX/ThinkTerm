# ThinkTerm

ThinkTerm is a macOS-focused terminal app based on WezTerm, with workspace and session management built into the main window. It is built for heavy AI workflows while maintaining high performance and low RAM usage.

## Build

```bash
make build BUILD_OPTS=--release
TAG_NAME=v0.1.0 bash ci/deploy.sh
```

This creates:

```text
ThinkTerm-macos-v0.1.0/ThinkTerm.app
ThinkTerm-macos-v0.1.0.zip
```

Upload the `.zip` file to GitHub Releases. The `.app` bundle is inside the zip.

## macOS Security Note

If macOS blocks the app, run this in Terminal:

```bash
sudo xattr -r -d com.apple.quarantine /Applications/ThinkTerm.app
```

Why: I can't afford Apple's developer certificate ($99/year), so macOS may block the unsigned app. This command removes the quarantine flag and lets it run. Only use this command on apps you trust.

## Notes

- The package script uses the binaries from `target/release/`.
- The macOS app template lives in `assets/macos/ThinkTerm.app`.
- Change `TAG_NAME` for each release version.
