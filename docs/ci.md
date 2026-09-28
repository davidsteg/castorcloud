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
| `ghcr.io/davidsteg/castorcloud-portal` | `apps/portal/Dockerfile` |
| `ghcr.io/davidsteg/castorcloud-provisioner` | `apps/provisioner/Dockerfile` |
| `ghcr.io/davidsteg/castorcloud-assistant-tools` | `agent/tools/Dockerfile` |
| `ghcr.io/davidsteg/castorcloud-cryptd` | `infra/node/cryptd/Dockerfile` |

Pull requests build and scan (amd64). Pushes to `main` publish amd64 and
arm64 images tagged with the full commit SHA and `main`, with an SBOM and
build provenance attached. Deploy by digest or SHA tag, never `main`.
Runtime images run as a non-root user and carry no npm.

The portal bakes `BASE_DOMAIN` (default `castorcloud.ch`) into its client
bundle; everything else is read from the environment at run time.

### Signing

Images are signed with cosign when the repository secret
`COSIGN_PRIVATE_KEY` is set. A key pair is used instead of keyless signing
because keyless writes this private repo's name to the public Rekor log.

```sh
cosign generate-key-pair           # writes cosign.key and cosign.pub
# Repo > Settings > Secrets and variables > Actions:
#   COSIGN_PRIVATE_KEY = contents of cosign.key
#   COSIGN_PASSWORD    = the password you chose
cosign verify --key cosign.pub --insecure-ignore-tlog \
  ghcr.io/davidsteg/castorcloud-portal@sha256:<digest>
```

Keep `cosign.key` offline; commit only `cosign.pub` if nodes should verify.

## Accepted findings

`.trivyignore` lists CVEs accepted for now, each with a reason and an
expiry date after which the scan fails again.
