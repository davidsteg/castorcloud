# CI, security gates and images

Three GitHub Actions workflows run on every pull request and on `main`.
Every third-party action is pinned to a commit SHA and every scanner image to
a digest; Dependabot (`.github/dependabot.yml`) proposes updates monthly (security updates right away).

| Workflow | Job | Fails when |
| --- | --- | --- |
| CI | Typecheck and test | typecheck or unit tests fail |
| CI | Rust core | `cargo fmt`, `clippy -D warnings`, tests or `cargo audit` fail |
| Security | CodeQL (javascript-typescript, rust, actions) | any `security-extended` finding |
| Security | npm audit | a PR adds a high or critical advisory (existing ones are listed, not blocking) |
| Security | Secret scan (gitleaks) | a secret appears anywhere in git history |
| Security | Dockerfile lint (hadolint) | a warning in any Dockerfile (`.hadolint.yaml`) |
| Images | Build and scan | Trivy finds a fixable high or critical CVE not in `.trivyignore` |

Security also runs weekly, so new advisories for unchanged code show up.
Code scanning uploads need GitHub Code Security on a private repo, so CodeQL
results are shown in the job summary instead of the Security tab.

## Images

| Image | Dockerfile |
| --- | --- |
| `ghcr.io/davidsteg/castorcloud` | `Dockerfile` |
| `ghcr.io/davidsteg/castorcloud-assistant-tools` | `agent/tools/Dockerfile` |
| `ghcr.io/davidsteg/castorcloud-egress` | `infra/node/egress/Dockerfile` |
| `ghcr.io/davidsteg/castorcloud-cryptd` | `infra/node/cryptd/Dockerfile` |

`castorcloud` holds the control plane's own services: the portal and the
Rust binaries (`castord`, `castor-monitor`, `castor-telegram`,
`castor-admin`). Run one container per service from it, each with only its
own secrets and mounts:

```sh
docker run ghcr.io/davidsteg/castorcloud@sha256:<digest>            # portal (port 3000)
docker run ghcr.io/davidsteg/castorcloud@sha256:<digest> castord    # also: monitor, telegram,
                                                                    # admin, migrate
```

Only castord and `admin` get the node SSH key (`/home/node/.ssh`,
read-only), the Traefik dynamic directory
(`/app/infra/control-plane/traefik/dynamic`) and the rendered tenant files
(`/app/.tenants`). The node images stay separate: cryptd runs privileged,
assistant-tools runs per tenant and egress on every stacks node, so none
of them should carry the control plane.

Pull requests build and scan (amd64). Pushes to `main` publish the images
tagged with the full commit SHA and `main`, with an SBOM and build
provenance attached, all for amd64; the automatic release of each merge
(below) is amd64 too. A release published by hand also builds the node
images for arm64 (Mac minis); main doesn't, because arm64 builds run
under emulation and take up to half an hour each. Deploy by digest or SHA tag, never `main`.
Runtime images run as a non-root user; the castor image carries no npm,
and the Rust node images are a static binary on an empty image.

The castor image bakes `BASE_DOMAIN` (default `castorcloud.ch`) into its client
bundle; everything else is read from the environment at run time.

### Signing

Images are signed with cosign (v2, key-based) when the repository secret
`COSIGN_PRIVATE_KEY` is set. Nodes only take signed images (below), so set
it before using `CASTOR_IMAGE_TAG`. A key pair is used instead of keyless signing
because keyless writes this private repo's name to the public Rekor log.

```sh
cosign generate-key-pair           # writes cosign.key and cosign.pub
# Repo > Settings > Secrets and variables > Actions:
#   COSIGN_PRIVATE_KEY = contents of cosign.key
#   COSIGN_PASSWORD    = the password you chose
cosign verify --key cosign.pub --insecure-ignore-tlog \
  ghcr.io/davidsteg/castorcloud@sha256:<digest>
```

Keep `cosign.key` offline; commit only `cosign.pub` if nodes should verify.

### Private packages

The packages inherit this private repository's access, so only accounts
with access to the repo can pull. Check once after the first publish:
GitHub > Your profile > Packages > each `castorcloud*` package > Package
settings: visibility **Private**, and under "Manage Actions access" only
`davidsteg/castorcloud`, with the Write role.

### Versions and releases

The version lives in one place, `VERSION` at the repository root
(`MAJOR.MINOR.PATCH`). Rules:

- **Every pull request raises `VERSION`** above `main`'s: patch for fixes and
  dependency bumps, minor for features, major for breaking changes. The
  Images workflow's `Version` check fails otherwise, Dependabot pull requests
  included (push a commit that raises it before merging).
- **Every merge to `main` is released.** The push builds, scans and publishes
  the images tagged `v<VERSION>` (plus the commit SHA and `main`), and once
  they are all published the `Release` job creates the GitHub release
  `v<VERSION>` on that commit with generated notes. A push whose
  `v<VERSION>` already exists fails instead of overwriting it.
- Runs on `main` are never cancelled by a newer push, so no merge is skipped.

The release is created with `GITHUB_TOKEN`, so it doesn't start the workflow
a second time. A release published by hand runs the same publish for its tag.

### How nodes get the images

In production castord no longer builds cryptd, assistant-tools and egress
on the nodes. For each node it:

1. resolves `castorcloud-<name>:$CASTOR_IMAGE_TAG` to a digest and checks
   the cosign signature against `CASTOR_COSIGN_PUBKEY`
   (`rust/crates/castord/src/images.rs`, no cosign binary). The signature
   must come from that key and name exactly that repository and digest, or
   nothing is pulled; each image is checked once per castord process;
2. has the node pull that digest (never the tag) and tags it locally
   (`pc-cryptd:1`, `pc-assistant-tools:2`, `castor.local/egress:2`).

The check runs on the control plane, so nodes need neither cosign nor
stored credentials: the docker CLI passes the pull token to the node's
engine for that one pull, from a temporary docker config (mode 0600) that
is removed right after.

Pull token: GitHub > Settings > Developer settings > Personal access tokens
> **Tokens (classic)** > Generate, with only the `read:packages` scope and an
expiry date (fine-grained tokens can't read GHCR yet). Put it in the
control plane's `.env`:

```sh
CASTOR_IMAGE_TAG=<commit SHA of a green main build>
CASTOR_COSIGN_PUBKEY=/run/secrets/cosign.pub   # cosign.pub from "Signing"
GHCR_PULL_USER=davidsteg
GHCR_PULL_TOKEN=ghp_...                        # read:packages only
```

With `CASTOR_IMAGE_TAG` unset, nodes build the images from the checkout, as
in development. Setting it without `CASTOR_COSIGN_PUBKEY` is an error, so
production never runs an unsigned image.

## Accepted findings

`.trivyignore` lists CVEs accepted for now, each with a reason and an
expiry date after which the scan fails again. `.github/codeql-accepted.txt`
does the same for CodeQL findings reviewed as intended, per rule and file.
