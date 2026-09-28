# CI, security gates and images

Three GitHub Actions workflows run on every pull request and on `main`.
Every third-party action is pinned to a commit SHA and every scanner image to
a digest; Dependabot (`.github/dependabot.yml`) proposes updates weekly.

| Workflow | Job | Fails when |
| --- | --- | --- |
| CI | Typecheck and test | typecheck or unit tests fail |
| Security | CodeQL (javascript-typescript, actions) | any `security-extended` finding |
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
| `ghcr.io/davidsteg/castorcloud-cryptd` | `infra/node/cryptd/Dockerfile` |

`castorcloud` holds all of Castor's own Node services. Run one container
per service from it, each with only its own secrets and mounts:

```sh
docker run ghcr.io/davidsteg/castorcloud@sha256:<digest>            # portal (port 3000)
docker run ghcr.io/davidsteg/castorcloud@sha256:<digest> worker     # also: monitor, telegram,
                                                                    # node, backup, migrate
```

Only the worker gets the node SSH key (`/home/node/.ssh`, read-only) and
the Traefik dynamic directory (`/app/infra/control-plane/traefik/dynamic`).
The two node images stay separate: cryptd runs privileged and
assistant-tools runs per tenant, so neither should carry the control plane.

Pull requests build and scan (amd64). Pushes to `main` publish amd64 and
arm64 images tagged with the full commit SHA and `main`, with an SBOM and
build provenance attached. Deploy by digest or SHA tag, never `main`.
Runtime images run as a non-root user and carry no npm.

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

### How nodes get the images

In production the provisioner no longer builds cryptd and assistant-tools
on the nodes. For each node it:

1. resolves `castorcloud-<name>:$CASTOR_IMAGE_TAG` to a digest and checks
   the cosign signature against `CASTOR_COSIGN_PUBKEY`
   (`apps/provisioner/src/verify-image.ts`). The signature must come from that
   key and name exactly that repository and digest, or nothing is pulled;
2. has the node pull that digest (never the tag) and tags it locally
   (`pc-cryptd:1`, `pc-assistant-tools:1`).

The check runs on the control plane, so nodes need neither cosign nor
stored credentials: the docker CLI passes the pull token to the node's
engine for that one pull (`apps/provisioner/src/images.ts`).

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
