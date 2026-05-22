# gpam

Terminal UI for [Google Cloud Privileged Access Manager](https://docs.cloud.google.com/iam/docs/pam-overview). Browse the entitlements you can request, submit grants with duration + justification, and watch them transition to Active.

![gpam demo](demo/gpam.gif)

## Quick start

Application Default Credentials must be available for non-demo runs:

```
gcloud auth application-default login
```


```
Usage: gpam [OPTIONS]

Options:
      --demo         Run with seeded fixtures instead of talking to GCP
      --no-projects  Skip project-scope entitlement discovery
      --no-folders   Skip folder-scope entitlement discovery
      --no-orgs      Skip organization-scope entitlement discovery
  -h, --help         Print help
  -V, --version      Print version
```

## Access model

1. Eligibility (PAM-side): to see and request an entitlement, you must be listed as a requester on it (directly or via a group).
2. Resource visibility (IAM-side): gpam enumerates the projects, folders, and organizations you can see and searches each for entitlements. That enumeration uses `resourcemanager.projects.get`, `resourcemanager.folders.get`, and `resourcemanager.organizations.get`. The standard way to grant these at the right scope is the Browser role (`roles/browser`). You can skip a scope tier with `--no-projects`, `--no-folders`, or `--no-orgs` if you don't have visibility there. Polling grant state uses `privilegedaccessmanager.grants.get`, which the grant's creator normally has on their own grants.

## Cache

Per-account SQLite at `~/Library/Caches/gpam/<account>.db` (macOS) or `~/.cache/gpam/<account>.db` (Linux). Soft freshness 30 m (background refresh), hard 24 h (block on refresh). Wipe with:

```
rm -f ~/Library/Caches/gpam/<account>.db
```
