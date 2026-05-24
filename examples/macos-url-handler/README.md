# gpam macOS URL handler

A small macOS app that catches GCP PAM approval links and forwards them to a
running `gpam` TUI via `gpam send`, falling back to `gpam approve` if no TUI
is open.

When you right-click a PAM approval link in Mail or Safari and choose
**Open With > Gpam Approve**, the app extracts the grant resource name from the
URL and passes it to gpam without you copying anything manually.

## Install

```sh
./examples/macos-url-handler/build.sh
open ~/Applications/Gpam\ Approve.app
```

Logs land in `/tmp/gpam-handler.log` if anything looks wrong.

## Uninstall

```sh
/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister \
    -u ~/Applications/Gpam\ Approve.app
rm -rf ~/Applications/Gpam\ Approve.app
```
